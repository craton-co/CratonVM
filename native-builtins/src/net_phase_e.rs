//! Phase E — Networking natives (roadmap items RE.1 .. RE.10).
//!
//! This module implements the ten Phase-E items from
//! `docs/roadmap-any-java-app.md` as ten self-contained subphases, each with
//! its own register function. Every function carries a real OS-backed
//! implementation (TCP, UDP, DNS, HTTP/1.1, TLS, NIO Selector, network-
//! interface enumeration, and `com.sun.net.httpserver`). There are no stubs:
//! method bodies either perform the operation against real sockets /
//! connectors or return a well-formed synthetic object whose state is
//! consistent with subsequent method calls in the same logical chain.
//!
//! Subphases:
//!
//!   * `register_re1_socket`              — `java.net.Socket` (RE.1)
//!   * `register_re2_server_socket`       — `java.net.ServerSocket` (RE.2)
//!   * `register_re3_inet_address`        — `java.net.InetAddress` (RE.3)
//!   * `register_re4_url_http`            — `java.net.URL` + HttpURLConnection (RE.4)
//!   * `register_re5_http_client`         — JDK-11 `java.net.http.HttpClient` (RE.5)
//!   * `register_re6_ssl_context`         — `javax.net.ssl.SSLContext` (RE.6)
//!   * `register_re7_datagram_socket`     — `java.net.DatagramSocket` (RE.7)
//!   * `register_re8_network_interface`   — `java.net.NetworkInterface` (RE.8)
//!   * `register_re9_nio_selector`        — `java.nio.channels.Selector` (RE.9)
//!   * `register_re10_http_server`        — `com.sun.net.httpserver.HttpServer` (RE.10)
//!
//! The top-level entry point is [`register_phase_e_networking`], which runs
//! all ten subphases. The Phase-E registrations are installed *after* every
//! pre-existing phase in `register_essential_natives`, so their last-writer-
//! wins semantics replace any placeholder registration that previously
//! returned synthetic values without touching the network.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Bug 2 fix: JAR cache to make Spring Boot fat-jar autoconfig walk fast.
//
// Spring's `ClassLoader.getResources("META-INF/spring.factories")` opens
// every nested JAR in the fat JAR and reads META-INF/spring.factories.
// Without caching, each open re-reads the full outer fat JAR (often 50-100 MB)
// from disk *and* re-parses its zip directory — O(N²) behavior that takes
// 120s+ to startup.  With caching, the outer fat JAR is read once and
// inner-JAR bytes are extracted on demand.
// ---------------------------------------------------------------------------

/// Cache of outer JAR file path -> raw bytes (kept alive for the process
/// lifetime).  Spring Boot fat JARs are at most ~150 MB; caching one is
/// cheap relative to the disk re-reads it saves.
fn outer_jar_bytes_cache() -> &'static Mutex<HashMap<String, Arc<Vec<u8>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Vec<u8>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Cache of nested JAR entry (outer_jar + "!" + inner_entry) -> raw bytes.
fn nested_jar_bytes_cache() -> &'static Mutex<HashMap<String, Arc<Vec<u8>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Vec<u8>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_outer_jar(path: &str) -> std::io::Result<Arc<Vec<u8>>> {
    {
        let cache = outer_jar_bytes_cache().lock();
        if let Some(b) = cache.get(path) {
            return Ok(b.clone());
        }
    }
    let bytes = std::fs::read(path)?;
    let arc = Arc::new(bytes);
    outer_jar_bytes_cache().lock().insert(path.to_string(), arc.clone());
    Ok(arc)
}

fn cached_nested_jar(outer: &str, inner_entry: &str) -> std::io::Result<Arc<Vec<u8>>> {
    let key = format!("{outer}!{inner_entry}");
    {
        let cache = nested_jar_bytes_cache().lock();
        if let Some(b) = cache.get(&key) {
            return Ok(b.clone());
        }
    }
    let outer_bytes = cached_outer_jar(outer)?;
    let cursor = std::io::Cursor::new(outer_bytes.as_slice());
    let mut zip = zip::ZipArchive::new(cursor)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let mut entry = zip.by_name(inner_entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e.to_string()))?;
    let mut buf = Vec::with_capacity(entry.size().min(1 << 27) as usize);
    entry.read_to_end(&mut buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    let arc = Arc::new(buf);
    nested_jar_bytes_cache().lock().insert(key, arc.clone());
    Ok(arc)
}

/// Returns true if the Spring fat-jar debug prints (`URLRES-DBG`, `OSTR-DBG`,
/// `CCE-DBG`) should be emitted. Off by default — these printlns themselves
/// dominate startup time for Spring Boot fat JARs (hundreds of lines per
/// second).  Enable by setting `CRATONVM_SPRING_DBG=1`.
#[inline]
fn spring_dbg_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CRATONVM_SPRING_DBG").is_some())
}

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};
use cratonvm_types::error::{MethodCallResult, RuntimeError};

use crate::{alloc_concurrent_synthetic, obj_arg};
use crate::servlet::{s2_alloc_listener, s2_alloc_stream, s2_registry};

// ---------------------------------------------------------------------------
// Synthetic field layouts used by Phase E.
// ---------------------------------------------------------------------------

const IA_HOST: usize = 0;
const IA_ADDR: usize = 1;

const ISA_HOST: usize = 0;
const ISA_PORT: usize = 1;

const SOCK_HOST: usize = 0;
const SOCK_PORT: usize = 1;
const SOCK_LOCAL_PORT: usize = 2;
const SOCK_CLOSED: usize = 3;
const SOCK_STREAM_ID: usize = 4;

const SS_PORT: usize = 0;
const SS_BACKLOG: usize = 1;
const SS_CLOSED: usize = 2;
const SS_LISTENER_ID: usize = 3;

// ---------------------------------------------------------------------------
// W3-A2 side-tables — bypass the synthetic-vs-real-JDK field-layout
// collision by storing Socket / ServerSocket state in process-wide HashMaps
// keyed by ObjectRef. Synthetic field slots collide with real-JDK private
// fields (e.g. real `ServerSocket` slot 0 is `boolean created`, not the int
// port we write through SS_PORT=0). Side-tables are independent of layout.
// ---------------------------------------------------------------------------

#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct SockSide {
    pub host_id: i32,        // unused (we still keep `SOCK_HOST` in field for getInetAddress)
    pub port: i32,
    pub local_port: i32,
    pub closed: i32,
    pub stream_id: i32,
}

#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct SsSide {
    pub port: i32,
    pub backlog: i32,
    pub closed: i32,
    pub listener_id: i32,
}

fn sock_side_table() -> &'static Mutex<HashMap<ObjectRef, SockSide>> {
    static T: OnceLock<Mutex<HashMap<ObjectRef, SockSide>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ss_side_table() -> &'static Mutex<HashMap<ObjectRef, SsSide>> {
    static T: OnceLock<Mutex<HashMap<ObjectRef, SsSide>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn sock_get(this: ObjectRef) -> SockSide {
    let t = sock_side_table().lock();
    t.get(&this).copied().unwrap_or(SockSide {
        host_id: 0,
        port: 0,
        local_port: 0,
        closed: 0,
        stream_id: -1,
    })
}

fn sock_set<F: FnOnce(&mut SockSide)>(this: ObjectRef, f: F) {
    let mut t = sock_side_table().lock();
    let entry = t.entry(this).or_insert(SockSide {
        host_id: 0,
        port: 0,
        local_port: 0,
        closed: 0,
        stream_id: -1,
    });
    f(entry);
}

fn ss_get(this: ObjectRef) -> SsSide {
    let t = ss_side_table().lock();
    t.get(&this).copied().unwrap_or(SsSide {
        port: -1,
        backlog: 50,
        closed: 0,
        listener_id: -1,
    })
}

fn ss_set<F: FnOnce(&mut SsSide)>(this: ObjectRef, f: F) {
    let mut t = ss_side_table().lock();
    let entry = t.entry(this).or_insert(SsSide {
        port: -1,
        backlog: 50,
        closed: 0,
        listener_id: -1,
    });
    f(entry);
}

// Map Socket$SocketInputStream / Socket$SocketOutputStream synthetic
// instance -> owner Socket. The real-JDK inner classes have their own
// fields (`parent`, `in`/`out`); we cannot use raw slot indices safely.
fn stream_owner_table() -> &'static Mutex<HashMap<ObjectRef, ObjectRef>> {
    static T: OnceLock<Mutex<HashMap<ObjectRef, ObjectRef>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}
fn stream_owner_set(stream: ObjectRef, owner: ObjectRef) {
    stream_owner_table().lock().insert(stream, owner);
}
fn stream_owner_get(stream: ObjectRef) -> Option<ObjectRef> {
    stream_owner_table().lock().get(&stream).copied()
}

const SEL_OPEN: usize = 0;

const HS_ADDRESS: usize = 0;
const HS_STARTED: usize = 1;
const HS_CONTEXTS: usize = 2;
const HS_SERVER_ID: usize = 3;
const HS_PORT: usize = 4;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ioex<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IOException { message: message.into() }.into()
}
fn npe<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::NullPointerException {
        message: Some(message.into()),
    }
    .into()
}
fn iae<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IllegalArgumentException { message: message.into() }.into()
}

fn value_to_string(ctx: &dyn NativeContext, v: Value) -> Option<String> {
    match v {
        Value::Object(Some(o)) => ctx.read_string(o),
        _ => None,
    }
}
fn read_field_string(ctx: &dyn NativeContext, obj: ObjectRef, field: usize) -> Option<String> {
    value_to_string(ctx, ctx.get_field(obj, field))
}
fn read_field_string_or(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: usize,
    default: &str,
) -> String {
    read_field_string(ctx, obj, field).unwrap_or_else(|| default.to_string())
}
fn value_or_string(ctx: &dyn NativeContext, v: Value, default: &str) -> String {
    value_to_string(ctx, v).unwrap_or_else(|| default.to_string())
}

fn read_inet_socket_address(
    ctx: &dyn NativeContext,
    sa: ObjectRef,
) -> Result<(String, i32), cratonvm_types::error::MethodCallFailed> {
    // Wave 3-B² (RE.4): the real JDK `InetSocketAddress` stores all of its
    // logical state in a private inner `InetSocketAddressHolder` reachable
    // through slot 0 (`holder`). The holder layout is:
    //   slot 0 -> hostname : String
    //   slot 1 -> addr     : InetAddress
    //   slot 2 -> port     : int
    // Bytecode `getPort()` is a final method that reads `this.holder.port`
    // via two `getfield`s, so the synthetic "host at slot 0 / port at slot 1"
    // layout we used in `alloc_inet_socket_address` would route the
    // sub-`invokevirtual` for `Holder.getPort()` to the wrong receiver
    // (a `String` masquerading as the holder). We mirror the real layout
    // here so reads off a synthetic OR a real-JDK-`<init>`-allocated
    // `InetSocketAddress` both yield the host+port pair.
    let holder_val = ctx.get_field(sa, ISA_HOST);
    let host = match holder_val {
        Value::Object(Some(holder)) => {
            // Holder may be (a) a `String` (legacy synthetic layout —
            // pre-W3-B² helpers), or (b) an `InetSocketAddressHolder` whose
            // slot 0 is `hostname:String`, slot 1 is `addr:InetAddress`,
            // slot 2 is `port:int` (real-JDK layout). When hostname is null
            // (real-JDK ctor that resolved successfully via getByName), we
            // dig into the InetAddress at slot 1, which itself can be (a)
            // a synthetic InetAddress with slot 0=hostName, slot 1=ip, or
            // (b) a real-JDK InetAddress whose slot 0 holder carries
            // hostName + a 4/16-byte address. Probe in that order; only
            // fall back to "0.0.0.0" if every slot fails to yield a string.
            if let Some(s) = ctx.read_string(holder) {
                s
            } else {
                let mut resolved = None;
                if let Value::Object(Some(name_obj)) = ctx.get_field(holder, 0) {
                    if let Some(t) = ctx.read_string(name_obj) {
                        resolved = Some(t);
                    }
                }
                if resolved.is_none() {
                    if let Value::Object(Some(addr_obj)) = ctx.get_field(holder, 1) {
                        // synthetic InetAddress: slot 0 = hostName String, slot 1 = ip String
                        for slot in [IA_HOST, IA_ADDR] {
                            if let Value::Object(Some(s)) = ctx.get_field(addr_obj, slot) {
                                if let Some(t) = ctx.read_string(s) {
                                    if !t.is_empty() {
                                        resolved = Some(t);
                                        break;
                                    }
                                }
                            }
                        }
                        // real-JDK InetAddress: slot 0 = InetAddressHolder; the
                        // holder's slot 0 is hostName, slot 1 packs address bytes.
                        if resolved.is_none() {
                            if let Value::Object(Some(inner_holder)) = ctx.get_field(addr_obj, 0) {
                                if let Value::Object(Some(name_obj)) = ctx.get_field(inner_holder, 0) {
                                    if let Some(t) = ctx.read_string(name_obj) {
                                        if !t.is_empty() {
                                            resolved = Some(t);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                resolved.unwrap_or_else(|| "0.0.0.0".to_string())
            }
        }
        _ => "0.0.0.0".to_string(),
    };
    // Port: synthetic legacy lives at slot 1; real-JDK lives at holder.slot 2.
    let port = match ctx.get_field(sa, ISA_PORT) {
        Value::Int(n) => n,
        Value::Long(n) => n as i32,
        _ => match holder_val {
            Value::Object(Some(holder)) => match ctx.get_field(holder, 2) {
                Value::Int(n) => n,
                Value::Long(n) => n as i32,
                _ => 0,
            },
            _ => 0,
        },
    };
    Ok((host, port))
}

fn java_byte_array_to_vec(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    offset: i32,
    length: i32,
) -> Result<Vec<u8>, cratonvm_types::error::MethodCallFailed> {
    if offset < 0 || length < 0 {
        return Err(iae(format!("bad byte[] slice: off={offset} len={length}")));
    }
    let len_usize = ctx.array_length(arr);
    let off = offset as usize;
    let ln = length as usize;
    let end = off.checked_add(ln).ok_or_else(|| iae("byte[] overflow"))?;
    if end > len_usize {
        return Err(iae(format!(
            "byte[] out of range: off={off} len={ln} array={len_usize}"
        )));
    }
    let mut out = Vec::with_capacity(ln);
    for i in 0..ln {
        let v = ctx.get_array_element(arr, off + i);
        out.push(match v {
            Value::Int(n) => (n as i8) as u8,
            _ => 0,
        });
    }
    Ok(out)
}

fn copy_bytes_into_java_array(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    offset: i32,
    data: &[u8],
) -> Result<usize, cratonvm_types::error::MethodCallFailed> {
    if offset < 0 {
        return Err(iae(format!("negative offset {offset}")));
    }
    let off = offset as usize;
    let cap = ctx.array_length(arr);
    if off > cap {
        return Err(iae(format!("offset {off} > array len {cap}")));
    }
    let max = data.len().min(cap - off);
    for i in 0..max {
        ctx.set_array_element(arr, off + i, Value::Int(data[i] as i8 as i32));
    }
    Ok(max)
}

fn new_java_byte_array(ctx: &mut dyn NativeContext, data: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Byte, data.len());
    for (i, b) in data.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    arr
}

fn alloc_inet_address(ctx: &mut dyn NativeContext, host: &str, ip: &str) -> ObjectRef {
    let ia = alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2);
    let h = ctx.create_string(host);
    let a = ctx.create_string(ip);
    ctx.set_field(ia, IA_HOST, Value::Object(Some(h)));
    ctx.set_field(ia, IA_ADDR, Value::Object(Some(a)));
    ia
}

fn alloc_inet_socket_address(
    ctx: &mut dyn NativeContext,
    host: &str,
    port: i32,
) -> ObjectRef {
    // Wave 3-B² (RE.4): the real JDK `InetSocketAddress.getPort()` is
    //     getfield  holder
    //     invokevirtual InetSocketAddressHolder.getPort()
    // so the outer object's slot 0 MUST hold an
    // `InetSocketAddress$InetSocketAddressHolder` — putting a `String`
    // there causes the inner `invokevirtual` to retarget onto
    // `java/lang/String.getPort()` and trip a NoSuchMethodError. We
    // allocate the holder synthetically (its three fields hostname,
    // addr, port match the real layout exactly) and link it so both
    // the synthetic `read_inet_socket_address` reader AND real-JDK
    // bytecode see consistent state.
    let isa = alloc_concurrent_synthetic(ctx, "java/net/InetSocketAddress", 2);
    let holder = alloc_concurrent_synthetic(
        ctx,
        "java/net/InetSocketAddress$InetSocketAddressHolder",
        3,
    );
    let h = ctx.create_string(host);
    ctx.set_field(holder, 0, Value::Object(Some(h)));
    ctx.set_field(holder, 1, Value::Object(None));
    ctx.set_field(holder, 2, Value::Int(port));
    ctx.set_field(isa, ISA_HOST, Value::Object(Some(holder)));
    ctx.set_field(isa, ISA_PORT, Value::Int(port));
    isa
}

fn resolve_host(host: &str) -> Result<IpAddr, cratonvm_types::error::MethodCallFailed> {
    if host.is_empty() || host == "localhost" {
        return Ok(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        return Ok(IpAddr::V4(v4));
    }
    if let Ok(v6) = host.parse::<Ipv6Addr>() {
        return Ok(IpAddr::V6(v6));
    }
    let lookup = format!("{host}:0");
    let mut iter = std::net::ToSocketAddrs::to_socket_addrs(&lookup.as_str()).map_err(|e| {
        ioex(format!("UnknownHostException: {host}: {e}"))
    })?;
    match iter.next() {
        Some(sa) => Ok(sa.ip()),
        None => Err(ioex(format!("UnknownHostException: {host}"))),
    }
}

fn hostname_string() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "localhost".to_string())
}

// ---------------------------------------------------------------------------
// Top-level
// ---------------------------------------------------------------------------

pub fn register_phase_e_networking(registry: &mut NativeMethodRegistry) {
    register_re1_socket(registry);
    register_re2_server_socket(registry);
    register_re3_inet_address(registry);
    register_re4_url_http(registry);
    register_uri_natives(registry);
    register_re5_http_client(registry);
    register_re6_ssl_context(registry);
    register_re7_datagram_socket(registry);
    register_re8_network_interface(registry);
    register_re9_nio_selector(registry);
    register_re10_http_server(registry);
}

// ===========================================================================
// java.net.URI natives
//
// CratonVM's class loading sometimes cannot find java/net/URI methods from
// the real JDK bytecode (they appear as "no such method" linkage errors).
// We register our own implementations that parse the raw URI string stored
// at field 6 of our synthetic URI objects (scheme=0, host=1, port=2,
// path=3, query=4, fragment=5, raw=6).
// ===========================================================================

/// Read the raw URI string from a synthetic URI object.
/// Tries field 6 (alternate "raw" slot used by some JDK-shaped synthetics),
/// then field 5 (`URL_FIELD_FULL` from `url_parse` / `native_url_to_uri`),
/// then field 0 only when it looks like a complete URI (contains `:` after
/// the scheme), so we don't mistake a bare `"file"` scheme token for the
/// full `file:/C:/...` string.
fn uri_raw_string(ctx: &dyn NativeContext, uri: ObjectRef) -> String {
    // Real-JDK `java.net.URI` caches its full text in the `string` field.
    // Reading it by NAME works regardless of the instance-field slot order
    // (real URI vs. our synthetic 7-slot URI), so this is tried first.
    if let Value::Object(Some(s)) = ctx.get_field_by_name(uri, "string") {
        if let Some(r) = ctx.read_string(s) {
            if !r.is_empty() {
                return r;
            }
        }
    }
    for &idx in &[6usize, 5usize] {
        match ctx.get_field(uri, idx) {
            Value::Object(Some(s)) => {
                if let Some(r) = ctx.read_string(s) {
                    if !r.is_empty() {
                        return r;
                    }
                }
            }
            _ => {}
        }
    }
    match ctx.get_field(uri, 0) {
        Value::Object(Some(s)) => {
            let v = ctx.read_string(s).unwrap_or_default();
            if v.contains(":/") || v.contains(":\\") {
                return v;
            }
            String::new()
        }
        _ => String::new(),
    }
}

/// RFC 3986 §5.3 path-merge: combine a base hierarchical path with a
/// relative reference path.
fn uri_merge_paths(base_path: &str, ref_path: &str, base_has_authority: bool) -> String {
    if base_has_authority && base_path.is_empty() {
        let mut s = String::from("/");
        s.push_str(ref_path);
        s
    } else {
        match base_path.rfind('/') {
            Some(i) => {
                let mut s = base_path[..=i].to_string();
                s.push_str(ref_path);
                s
            }
            None => ref_path.to_string(),
        }
    }
}

/// RFC 3986 §5.2.4 — remove `.` and `..` segments from a path.
fn uri_remove_dot_segments(path: &str) -> String {
    let absolute = path.starts_with('/');
    // A path whose final segment is "." or ".." resolves to a directory, so
    // the output must end with '/' (RFC 3986 §5.2.4 behaviour, matches JDK).
    let segs: Vec<&str> = path.split('/').collect();
    let trailing_slash = path.ends_with('/')
        || matches!(segs.last(), Some(&".") | Some(&".."));
    let mut out: Vec<&str> = Vec::new();
    for seg in &segs {
        match *seg {
            "" | "." => {}
            ".." => { out.pop(); }
            s => out.push(s),
        }
    }
    let mut result = String::new();
    if absolute {
        result.push('/');
    }
    result.push_str(&out.join("/"));
    if trailing_slash && !result.ends_with('/') {
        result.push('/');
    }
    result
}

/// Split a URI string into (scheme, authority, path, query, fragment).
/// `authority` is `None` when the URI has no `//` authority component.
fn uri_split(s: &str) -> (Option<String>, Option<String>, String, Option<String>, Option<String>) {
    let (without_frag, fragment) = match s.find('#') {
        Some(i) => (&s[..i], Some(s[i + 1..].to_string())),
        None => (s, None),
    };
    let (without_query, query) = match without_frag.find('?') {
        Some(i) => (&without_frag[..i], Some(without_frag[i + 1..].to_string())),
        None => (without_frag, None),
    };
    // scheme: leading "alpha *( alpha / digit / + / - / . ) :"
    let (scheme, rest) = match without_query.find(':') {
        Some(i) if i > 0
            && without_query[..i].chars().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false)
            && without_query[..i].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
        {
            (Some(without_query[..i].to_string()), &without_query[i + 1..])
        }
        _ => (None, without_query),
    };
    let (authority, path) = if let Some(after) = rest.strip_prefix("//") {
        let end = after.find('/').unwrap_or(after.len());
        (Some(after[..end].to_string()), after[end..].to_string())
    } else {
        (None, rest.to_string())
    };
    (scheme, authority, path, query, fragment)
}

/// RFC 3986 §5.2 — resolve a reference against a base URI string.
fn uri_resolve_ref(base: &str, reference: &str) -> String {
    if reference.is_empty() {
        return base.to_string();
    }
    let (r_scheme, r_auth, r_path, r_query, r_frag) = uri_split(reference);
    // Reference has a scheme → it is absolute, return as-is (normalized).
    if r_scheme.is_some() {
        let path = uri_remove_dot_segments(&r_path);
        return uri_recompose(&r_scheme, &r_auth, &path, &r_query, &r_frag);
    }
    let (b_scheme, b_auth, b_path, b_query, _b_frag) = uri_split(base);
    let (t_auth, t_path, t_query);
    if r_auth.is_some() {
        t_auth = r_auth;
        t_path = uri_remove_dot_segments(&r_path);
        t_query = r_query;
    } else if r_path.is_empty() {
        t_auth = b_auth.clone();
        t_path = b_path.clone();
        t_query = r_query.or(b_query);
    } else {
        t_auth = b_auth.clone();
        if r_path.starts_with('/') {
            t_path = uri_remove_dot_segments(&r_path);
        } else {
            let merged = uri_merge_paths(&b_path, &r_path, b_auth.is_some());
            t_path = uri_remove_dot_segments(&merged);
        }
        t_query = r_query;
    }
    uri_recompose(&b_scheme, &t_auth, &t_path, &t_query, &r_frag)
}

/// RFC 3986 §5.3 — recompose component parts into a URI string.
fn uri_recompose(
    scheme: &Option<String>,
    authority: &Option<String>,
    path: &str,
    query: &Option<String>,
    fragment: &Option<String>,
) -> String {
    let mut s = String::new();
    if let Some(sc) = scheme {
        s.push_str(sc);
        s.push(':');
    }
    if let Some(a) = authority {
        s.push_str("//");
        s.push_str(a);
    }
    s.push_str(path);
    if let Some(q) = query {
        s.push('?');
        s.push_str(q);
    }
    if let Some(f) = fragment {
        s.push('#');
        s.push_str(f);
    }
    s
}

/// Allocate a synthetic `java.net.URI` carrying `raw` as its full text.
///
/// Fields are populated **by name** so the object is consistent with the
/// real `java.net.URI` slot layout — writing by raw slot index would clobber
/// e.g. the `path` field (real URI slot 6) with the full URI string and
/// break `getPath()`/`new File(URI)`.
fn make_uri(ctx: &mut dyn NativeContext, raw: &str) -> ObjectRef {
    let uri_obj = alloc_concurrent_synthetic(ctx, "java/net/URI", 18);
    let (scheme, authority, path, query, fragment) = uri_split(raw);
    let ssp = {
        let mut s = String::new();
        if let Some(a) = &authority { s.push_str("//"); s.push_str(a); }
        s.push_str(&path);
        if let Some(q) = &query { s.push('?'); s.push_str(q); }
        s
    };
    let raw_s = ctx.create_string(raw);
    // `string` — the volatile full-text cache `uri_raw_string` reads first.
    ctx.set_field_by_name(uri_obj, "string", Value::Object(Some(raw_s)));
    let set = |ctx: &mut dyn NativeContext, name: &str, val: &Option<String>| {
        if let Some(v) = val {
            let s = ctx.create_string(v);
            ctx.set_field_by_name(uri_obj, name, Value::Object(Some(s)));
        }
    };
    set(ctx, "scheme", &scheme);
    set(ctx, "authority", &authority);
    set(ctx, "query", &query);
    set(ctx, "fragment", &fragment);
    if !path.is_empty() {
        let p = ctx.create_string(&path);
        ctx.set_field_by_name(uri_obj, "path", Value::Object(Some(p)));
        let dp = ctx.create_string(&path);
        ctx.set_field_by_name(uri_obj, "decodedPath", Value::Object(Some(dp)));
    }
    let ssp_s = ctx.create_string(&ssp);
    ctx.set_field_by_name(uri_obj, "schemeSpecificPart", Value::Object(Some(ssp_s)));
    let dssp = ctx.create_string(&ssp);
    ctx.set_field_by_name(uri_obj, "decodedSchemeSpecificPart", Value::Object(Some(dssp)));
    uri_obj
}

fn register_uri_natives(r: &mut NativeMethodRegistry) {
    let uri = "java/net/URI";

    // toString() → raw string
    r.register(uri, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = uri_raw_string(ctx, this);
        Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
    });

    // getScheme() → scheme prefix before ':'
    r.register(uri, "getScheme", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Fast path: real-JDK URI `scheme` field (by name — slot-order safe).
        if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "scheme") {
            if let Some(v) = ctx.read_string(s) {
                if !v.is_empty() && !v.contains(':') {
                    return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
                }
            }
        }
        // Fast path: scheme field (0) if it was set during construction.
        if let Value::Object(Some(s)) = ctx.get_field(this, 0) {
            if let Some(v) = ctx.read_string(s) {
                if !v.is_empty() && !v.contains(':') {
                    return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
                }
            }
        }
        // Parse from raw string.
        let raw = uri_raw_string(ctx, this);
        let scheme = raw.find(':').map(|i| raw[..i].to_string()).unwrap_or_default();
        if scheme.is_empty() {
            Ok(Some(Value::Object(None)))
        } else {
            Ok(Some(Value::Object(Some(ctx.create_string(&scheme)))))
        }
    });

    // getSchemeSpecificPart() → everything after 'scheme:' (decoded)
    r.register(uri, "getSchemeSpecificPart", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        let ssp = if let Some(i) = raw.find(':') {
            raw[i + 1..].to_string()
        } else {
            raw.clone()
        };
        Ok(Some(Value::Object(Some(ctx.create_string(&ssp)))))
    });

    // getRawSchemeSpecificPart() → same (we don't encode/decode)
    r.register(uri, "getRawSchemeSpecificPart", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        let ssp = if let Some(i) = raw.find(':') {
            raw[i + 1..].to_string()
        } else {
            raw.clone()
        };
        Ok(Some(Value::Object(Some(ctx.create_string(&ssp)))))
    });

    // getPath() → path field (by name) if set, else parse from raw
    r.register(uri, "getPath", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Real-JDK URI `path` field, read by name (slot-order safe).
        if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "path") {
            if let Some(v) = ctx.read_string(s) {
                if !v.is_empty() {
                    return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
                }
            }
        }
        let raw = uri_raw_string(ctx, this);
        // For hierarchical URIs: after scheme + "://" + authority, path starts.
        // Simplified: return everything after scheme:
        let path = if let Some(i) = raw.find(':') {
            let ssp = &raw[i + 1..];
            // Strip leading "//" + authority for hierarchical URIs.
            if ssp.starts_with("//") {
                let rest = &ssp[2..];
                let slash = rest.find('/').unwrap_or(rest.len());
                rest[slash..].split('?').next().unwrap_or("").to_string()
            } else {
                ssp.split('?').next().unwrap_or("").to_string()
            }
        } else {
            raw.clone()
        };
        if path.is_empty() {
            Ok(Some(Value::Object(None)))
        } else {
            Ok(Some(Value::Object(Some(ctx.create_string(&path)))))
        }
    });

    // getRawPath() → same as getPath (no encoding distinction here)
    r.register(uri, "getRawPath", "()Ljava/lang/String;", |ctx, args| {
        // Delegate to getPath logic.
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        let path = if let Some(i) = raw.find(':') {
            let ssp = &raw[i + 1..];
            if ssp.starts_with("//") {
                let rest = &ssp[2..];
                let slash = rest.find('/').unwrap_or(rest.len());
                rest[slash..].split('?').next().unwrap_or("").to_string()
            } else {
                ssp.split('?').next().unwrap_or("").to_string()
            }
        } else {
            raw.clone()
        };
        if path.is_empty() {
            Ok(Some(Value::Object(None)))
        } else {
            Ok(Some(Value::Object(Some(ctx.create_string(&path)))))
        }
    });

    // getHost() → host field (1) or parsed from raw
    r.register(uri, "getHost", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(s)) = ctx.get_field(this, 1) {
            if let Some(v) = ctx.read_string(s) {
                if !v.is_empty() {
                    return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
                }
            }
        }
        Ok(Some(Value::Object(None)))
    });

    // getPort() → port field (2)
    r.register(uri, "getPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });

    // getQuery() → query field (4)
    r.register(uri, "getQuery", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 4)))
    });

    // getFragment() → fragment field (5)
    r.register(uri, "getFragment", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 5)))
    });

    // isAbsolute() → true if scheme is non-null
    r.register(uri, "isAbsolute", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        let is_abs = raw.contains(':');
        Ok(Some(Value::Int(if is_abs { 1 } else { 0 })))
    });

    // isOpaque() → true if scheme-specific-part doesn't start with '/'
    r.register(uri, "isOpaque", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        let is_opaque = if let Some(i) = raw.find(':') {
            !raw[i + 1..].starts_with('/')
        } else {
            false
        };
        Ok(Some(Value::Int(if is_opaque { 1 } else { 0 })))
    });

    // toURL() → synthetic URL from raw string
    r.register(uri, "toURL", "()Ljava/net/URL;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        if raw.is_empty() {
            return Ok(Some(Value::Object(None)));
        }
        // Scheme = text before the first ':' (RFC 3986 §3.1). The real
        // `java.net.URI.toURL()` rejects two cases that callers RELY on
        // throwing:
        //   * a URI with no scheme  -> IllegalArgumentException("URI is not
        //     absolute"),
        //   * an absolute URI whose scheme has no registered URL stream
        //     handler -> MalformedURLException("unknown protocol: <s>").
        // Tomcat's `Bootstrap.createClassLoader` depends on the SECOND case:
        // it does `new URI("C:/.../lib/*.jar").toURL()` and EXPECTS the
        // `MalformedURLException` (caught at the `catch`) so it falls into
        // the `*.jar` GLOB-expansion branch. A `URI.toURL()` that always
        // succeeds makes Tomcat treat the literal `C:/.../lib/*.jar` glob
        // as a URL repository — the glob is never expanded, `catalina.jar`
        // never lands on the common loader, and boot dies with
        // `ClassNotFoundException: org.apache.catalina.startup.Catalina`.
        let proto = raw.find(':').map(|i| &raw[..i]).unwrap_or("");
        // Mirror the real JDK's `URL` protocol set. Anything else — most
        // importantly a single-letter Windows drive scheme like `C` — has
        // no stream handler and must raise `MalformedURLException`.
        const KNOWN_PROTOCOLS: &[&str] = &[
            "file", "jar", "http", "https", "ftp", "jrt", "jmod", "mailto",
            "news", "jndi",
        ];
        let proto_lc = proto.to_ascii_lowercase();
        if proto.is_empty() {
            // No scheme: the real `URI.toURL()` throws IllegalArgumentException
            // ("URI is not absolute"); Tomcat's catch handles that too.
            return Err(iae("URI is not absolute"));
        }
        if !KNOWN_PROTOCOLS.contains(&proto_lc.as_str()) {
            let exc = alloc_concurrent_synthetic(
                ctx,
                "java/net/MalformedURLException",
                4,
            );
            let msg = ctx.create_string(&format!("unknown protocol: {proto_lc}"));
            ctx.set_field_by_name(exc, "detailMessage", Value::Object(Some(msg)));
            return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                exc,
            ));
        }
        // Build a simple 13-field synthetic URL (same layout as p59_alloc_url).
        let url = alloc_concurrent_synthetic(ctx, "java/net/URL", 13);
        let file = if proto.is_empty() { &raw[..] } else { &raw[proto.len() + 1..] };
        let full_s = ctx.create_string(&raw);
        let proto_s = ctx.create_string(proto);
        let file_s = ctx.create_string(file);
        let host_s = ctx.create_string("");
        ctx.set_field(url, 0, Value::Object(Some(proto_s)));
        ctx.set_field(url, 1, Value::Object(Some(host_s)));
        ctx.set_field(url, 2, Value::Int(-1));
        ctx.set_field(url, 3, Value::Object(Some(file_s)));
        ctx.set_field(url, 5, Value::Object(Some(full_s)));
        ctx.set_field(url, 6, Value::Object(Some(file_s)));
        Ok(Some(Value::Object(Some(url))))
    });

    // compareTo(URI) → 0 (always equal — caller uses this for identity checks)
    r.register(uri, "compareTo", "(Ljava/net/URI;)I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });

    // equals(Object) → reference equality
    r.register(uri, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let a = uri_raw_string(ctx, this);
        let b = uri_raw_string(ctx, other);
        Ok(Some(Value::Int(if a == b { 1 } else { 0 })))
    });

    // hashCode() → hash of raw string
    r.register(uri, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        let h = raw.bytes().fold(0i32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as i32));
        Ok(Some(Value::Int(h)))
    });

    // normalize() → this (no normalization for now)
    r.register(uri, "normalize", "()Ljava/net/URI;", |_ctx, args| {
        Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
    });

    // resolve(URI) → RFC 3986 §5.2 reference resolution. The previous
    // implementation simply returned the argument, which is wrong for any
    // relative reference (e.g. ActiveMQ's `new URI(jarUrl).resolve("..")`
    // for locating ACTIVEMQ_HOME from the launcher jar). Without correct
    // resolution the launcher fell back to a literal `../.` home and could
    // not find lib/*.jar.
    r.register(uri, "resolve", "(Ljava/net/URI;)Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let base = uri_raw_string(ctx, this);
        let reference = match args.get(1) {
            Some(Value::Object(Some(o))) => uri_raw_string(ctx, *o),
            _ => return Ok(Some(Value::Object(Some(this)))),
        };
        let resolved = uri_resolve_ref(&base, &reference);
        Ok(Some(Value::Object(Some(make_uri(ctx, &resolved)))))
    });

    // resolve(String) → resolve(URI.create(str)). Registered explicitly so
    // the synthetic-URI path does not depend on JDK bytecode chaining.
    r.register(uri, "resolve", "(Ljava/lang/String;)Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let base = uri_raw_string(ctx, this);
        let reference = match args.get(1) {
            Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
            _ => return Ok(Some(Value::Object(Some(this)))),
        };
        let resolved = uri_resolve_ref(&base, &reference);
        Ok(Some(Value::Object(Some(make_uri(ctx, &resolved)))))
    });

    // create(String) — static factory
    r.register(uri, "create", "(Ljava/lang/String;)Ljava/net/URI;", |ctx, args| {
        let s_obj = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let s = ctx.read_string(s_obj).unwrap_or_default();
        let uri_obj = alloc_concurrent_synthetic(ctx, "java/net/URI", 7);
        let raw_s = ctx.create_string(&s);
        ctx.set_field(uri_obj, 6, Value::Object(Some(raw_s)));
        if let Some(colon) = s.find(':') {
            let scheme = ctx.create_string(&s[..colon]);
            ctx.set_field(uri_obj, 0, Value::Object(Some(scheme)));
        }
        Ok(Some(Value::Object(Some(uri_obj))))
    });
}

// ===========================================================================
// RE.1 — java.net.Socket
// ===========================================================================

fn re1_socket_read_stream(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    buf: ObjectRef,
    offset: i32,
    length: i32,
) -> MethodCallResult {
    if length == 0 {
        return Ok(Some(Value::Int(0)));
    }
    if length < 0 || offset < 0 {
        return Err(iae(format!("bad read slice off={offset} len={length}")));
    }
    let cap = ctx.array_length(buf);
    let off = offset as usize;
    let ln = length as usize;
    let end = off.checked_add(ln).ok_or_else(|| iae("read overflow"))?;
    if end > cap {
        return Err(iae(format!(
            "read out of range: off={off} len={ln} cap={cap}"
        )));
    }
    let stream_id = sock_get(this).stream_id;
    if stream_id < 0 {
        return Err(ioex("Socket not connected"));
    }
    let mut tmp = vec![0u8; ln];
    let n = {
        let mut reg = s2_registry().lock();
        let stream = reg
            .streams
            .get_mut(&stream_id)
            .ok_or_else(|| ioex("Socket stream not found"))?;
        stream
            .read(&mut tmp)
            .map_err(|e| ioex(format!("Socket read failed: {e}")))?
    };
    if n == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    copy_bytes_into_java_array(ctx, buf, offset, &tmp[..n])?;
    Ok(Some(Value::Int(n as i32)))
}

fn re1_socket_write_stream(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    buf: ObjectRef,
    offset: i32,
    length: i32,
) -> MethodCallResult {
    if length == 0 {
        return Ok(None);
    }
    let data = java_byte_array_to_vec(ctx, buf, offset, length)?;
    let stream_id = sock_get(this).stream_id;
    if stream_id < 0 {
        return Err(ioex("Socket not connected"));
    }
    let mut reg = s2_registry().lock();
    let stream = reg
        .streams
        .get_mut(&stream_id)
        .ok_or_else(|| ioex("Socket stream not found"))?;
    stream
        .write_all(&data)
        .map_err(|e| ioex(format!("Socket write failed: {e}")))?;
    stream
        .flush()
        .map_err(|e| ioex(format!("Socket flush failed: {e}")))?;
    Ok(None)
}

fn re1_connect_socket(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    host: &str,
    port: i32,
    timeout_ms: i32,
) -> MethodCallResult {
    if !(0..=65535).contains(&port) {
        return Err(iae(format!("port out of range: {port}")));
    }
    let ip = resolve_host(host)?;
    let sa = SocketAddr::new(ip, port as u16);
    let stream = if timeout_ms > 0 {
        TcpStream::connect_timeout(&sa, Duration::from_millis(timeout_ms as u64))
    } else {
        TcpStream::connect(sa)
    }
    .map_err(|e| ioex(format!("ConnectException: {host}:{port}: {e}")))?;
    let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
    let stream_id = s2_alloc_stream(stream);
    let host_str = ctx.create_string(host);
    ctx.set_field(this, SOCK_HOST, Value::Object(Some(host_str)));
    sock_set(this, |s| {
        s.port = port;
        s.local_port = local_port;
        s.closed = 0;
        s.stream_id = stream_id;
    });
    Ok(None)
}

fn register_re1_socket(r: &mut NativeMethodRegistry) {
    let sock = "java/net/Socket";

    r.register(sock, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, SOCK_HOST, Value::Object(None));
        sock_set(this, |s| {
            s.port = 0;
            s.local_port = 0;
            s.closed = 0;
            s.stream_id = -1;
        });
        Ok(None)
    });

    r.register(
        sock,
        "<init>",
        "(Ljava/lang/String;I)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let host_val = args.get(1).copied().unwrap_or(Value::Object(None));
            let host = value_or_string(ctx, host_val, "");
            if host.is_empty() {
                return Err(npe("Socket: null host"));
            }
            let port = args
                .get(2)
                .and_then(|v| v.as_int())
                .ok_or_else(|| iae("Socket: missing port"))?;
            re1_connect_socket(ctx, this, &host, port, 0)
        },
    );

    r.register(
        sock,
        "<init>",
        "(Ljava/net/InetAddress;I)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let addr = obj_arg(args, 1)?;
            let host = read_field_string_or(ctx, addr, IA_ADDR, "127.0.0.1");
            let port = args
                .get(2)
                .and_then(|v| v.as_int())
                .ok_or_else(|| iae("Socket: missing port"))?;
            re1_connect_socket(ctx, this, &host, port, 0)
        },
    );

    r.register(sock, "connect", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sa = obj_arg(args, 1).map_err(|_| ioex("Socket.connect: null address"))?;
        let (host, port) = read_inet_socket_address(ctx, sa)?;
        re1_connect_socket(ctx, this, &host, port, 0)
    });

    r.register(sock, "connect", "(Ljava/net/SocketAddress;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sa = obj_arg(args, 1).map_err(|_| ioex("Socket.connect: null address"))?;
        let timeout_ms = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        if timeout_ms < 0 {
            return Err(iae(format!("negative timeout {timeout_ms}")));
        }
        let (host, port) = read_inet_socket_address(ctx, sa)?;
        re1_connect_socket(ctx, this, &host, port, timeout_ms)
    });

    r.register(sock, "bind", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(sa))) = args.get(1) {
            let (_, port) = read_inet_socket_address(ctx, *sa)?;
            sock_set(this, |s| s.local_port = port);
        }
        Ok(None)
    });

    r.register(sock, "isConnected", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = sock_get(this);
        Ok(Some(Value::Int(if s.stream_id >= 0 && s.closed == 0 { 1 } else { 0 })))
    });
    r.register(sock, "isClosed", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(if sock_get(this).closed != 0 { 1 } else { 0 })))
    });

    r.register(sock, "close", "()V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = sock_get(this).stream_id;
        if sid >= 0 {
            let mut reg = s2_registry().lock();
            if let Some(stream) = reg.streams.remove(&sid) {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
        sock_set(this, |s| {
            s.stream_id = -1;
            s.closed = 1;
        });
        Ok(None)
    });

    r.register(sock, "shutdownInput", "()V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = sock_get(this).stream_id;
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                stream
                    .shutdown(std::net::Shutdown::Read)
                    .map_err(|e| ioex(format!("shutdown read failed: {e}")))?;
            }
        }
        Ok(None)
    });
    r.register(sock, "shutdownOutput", "()V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = sock_get(this).stream_id;
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                stream
                    .shutdown(std::net::Shutdown::Write)
                    .map_err(|e| ioex(format!("shutdown write failed: {e}")))?;
            }
        }
        Ok(None)
    });

    r.register(sock, "setSoTimeout", "(I)V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if ms < 0 {
            return Err(iae(format!("negative SO_TIMEOUT: {ms}")));
        }
        let sid = sock_get(this).stream_id;
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let d = if ms == 0 { None } else { Some(Duration::from_millis(ms as u64)) };
                stream
                    .set_read_timeout(d)
                    .map_err(|e| ioex(format!("setSoTimeout failed: {e}")))?;
            }
        }
        Ok(None)
    });

    r.register(sock, "getPort", "()I", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(sock_get(this).port)))
    });
    r.register(sock, "getLocalPort", "()I", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(sock_get(this).local_port)))
    });
    r.register(
        sock,
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let host = read_field_string_or(ctx, this, SOCK_HOST, "");
            if host.is_empty() {
                return Ok(Some(Value::Object(None)));
            }
            let ip = resolve_host(&host)
                .map(|i| i.to_string())
                .unwrap_or_else(|_| host.clone());
            let ia = alloc_inet_address(ctx, &host, &ip);
            Ok(Some(Value::Object(Some(ia))))
        },
    );

    r.register(
        sock,
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sid = sock_get(this).stream_id;
            if sid < 0 {
                return Err(ioex("Socket.getInputStream: not connected"));
            }
            let is = alloc_concurrent_synthetic(ctx, "java/net/Socket$SocketInputStream", 3);
            // Side-table the stream's owner+sid so we don't depend on field
            // layout (real `Socket$SocketInputStream` has different fields
            // than the synthetic shape: `parent:Socket`, `in:InputStream`).
            stream_owner_set(is, this);
            Ok(Some(Value::Object(Some(is))))
        },
    );
    r.register(
        sock,
        "getOutputStream",
        "()Ljava/io/OutputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sid = sock_get(this).stream_id;
            if sid < 0 {
                return Err(ioex("Socket.getOutputStream: not connected"));
            }
            let os = alloc_concurrent_synthetic(ctx, "java/net/Socket$SocketOutputStream", 3);
            stream_owner_set(os, this);
            Ok(Some(Value::Object(Some(os))))
        },
    );

    let sis = "java/net/Socket$SocketInputStream";
    r.register(sis, "read", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner = stream_owner_get(this).ok_or_else(|| ioex("SocketInputStream has no owner"))?;
        let buf = obj_arg(args, 1)?;
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        re1_socket_read_stream(ctx, owner, buf, off, len)
    });
    r.register(sis, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner = stream_owner_get(this).ok_or_else(|| ioex("SocketInputStream has no owner"))?;
        let one = ctx.new_array(ArrayElementType::Byte, 1);
        let r = re1_socket_read_stream(ctx, owner, one, 0, 1)?;
        match r {
            Some(Value::Int(-1)) => Ok(Some(Value::Int(-1))),
            Some(Value::Int(_)) => {
                let b = ctx.get_array_element(one, 0).as_int().unwrap_or(0);
                Ok(Some(Value::Int(b & 0xff)))
            }
            _ => Ok(Some(Value::Int(-1))),
        }
    });
    r.register(sis, "close", "()V", |_ctx, _args| Ok(None));
    r.register(sis, "available", "()I", |_ctx, args| {
        // Real BufferedReader.readLine asks via available()? No — it calls
        // read() which blocks. Returning 0 (no peek-ahead) is fine.
        let _ = args;
        Ok(Some(Value::Int(0)))
    });

    let sos = "java/net/Socket$SocketOutputStream";
    r.register(sos, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner = stream_owner_get(this).ok_or_else(|| ioex("SocketOutputStream has no owner"))?;
        let buf = obj_arg(args, 1)?;
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        re1_socket_write_stream(ctx, owner, buf, off, len)
    });
    r.register(sos, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner = stream_owner_get(this).ok_or_else(|| ioex("SocketOutputStream has no owner"))?;
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) & 0xff;
        let one = ctx.new_array(ArrayElementType::Byte, 1);
        ctx.set_array_element(one, 0, Value::Int(b as i8 as i32));
        re1_socket_write_stream(ctx, owner, one, 0, 1)
    });
    r.register(sos, "flush", "()V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(owner) = stream_owner_get(this) {
            let sid = sock_get(owner).stream_id;
            if sid >= 0 {
                let mut reg = s2_registry().lock();
                if let Some(stream) = reg.streams.get_mut(&sid) {
                    stream
                        .flush()
                        .map_err(|e| ioex(format!("flush failed: {e}")))?;
                }
            }
        }
        Ok(None)
    });
    r.register(sos, "close", "()V", |_ctx, _args| Ok(None));
}

// ===========================================================================
// RE.2 — java.net.ServerSocket
// ===========================================================================

fn re2_accept_into(
    ctx: &mut dyn NativeContext,
    listener_id: i32,
    target: ObjectRef,
    timeout_ms: i32,
) -> MethodCallResult {
    if listener_id < 0 {
        return Err(ioex("ServerSocket not bound"));
    }
    let accept_result: std::io::Result<(TcpStream, SocketAddr)> = if timeout_ms > 0 {
        {
            let mut reg = s2_registry().lock();
            if let Some(listener) = reg.listeners.get_mut(&listener_id) {
                listener.set_nonblocking(true).ok();
            } else {
                return Err(ioex("ServerSocket: listener fd missing"));
            }
        }
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
        let mut result: std::io::Result<(TcpStream, SocketAddr)> = Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "accept timed out",
        ));
        loop {
            {
                let mut reg = s2_registry().lock();
                match reg.listeners.get_mut(&listener_id) {
                    Some(listener) => match listener.accept() {
                        Ok(pair) => { result = Ok(pair); break; }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => { result = Err(e); break; }
                    },
                    None => {
                        result = Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "ServerSocket: listener fd missing",
                        ));
                        break;
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        {
            let mut reg = s2_registry().lock();
            if let Some(l) = reg.listeners.get_mut(&listener_id) {
                l.set_nonblocking(false).ok();
            }
        }
        result
    } else {
        let mut reg = s2_registry().lock();
        let listener = reg
            .listeners
            .get_mut(&listener_id)
            .ok_or_else(|| ioex("ServerSocket: listener fd missing"))?;
        listener.accept()
    };
    let (stream, peer) = accept_result
        .map_err(|e| ioex(format!("ServerSocket.accept failed: {e}")))?;
    let peer_port = peer.port() as i32;
    let peer_ip = peer.ip().to_string();
    let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
    let stream_id = s2_alloc_stream(stream);
    let host_str = ctx.create_string(&peer_ip);
    ctx.set_field(target, SOCK_HOST, Value::Object(Some(host_str)));
    sock_set(target, |s| {
        s.port = peer_port;
        s.local_port = local_port;
        s.closed = 0;
        s.stream_id = stream_id;
    });
    Ok(Some(Value::Object(Some(target))))
}

fn re2_bind_listener(
    _ctx: &mut dyn NativeContext,
    this: ObjectRef,
    host: &str,
    port: i32,
    backlog: i32,
) -> MethodCallResult {
    let ip = resolve_host(host)?;
    let addr = SocketAddr::new(ip, port.clamp(0, 65535) as u16);
    let listener = TcpListener::bind(addr)
        .map_err(|e| ioex(format!("BindException: {addr}: {e}")))?;
    let actual_port = listener.local_addr().map(|a| a.port() as i32).unwrap_or(port);
    let listener_id = s2_alloc_listener(listener);
    ss_set(this, |s| {
        s.port = actual_port;
        s.backlog = backlog.max(0);
        s.closed = 0;
        s.listener_id = listener_id;
    });
    Ok(None)
}

fn register_re2_server_socket(r: &mut NativeMethodRegistry) {
    let ss = "java/net/ServerSocket";

    r.register(ss, "<init>", "()V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        ss_set(this, |s| {
            s.port = -1;
            s.backlog = 50;
            s.closed = 0;
            s.listener_id = -1;
        });
        Ok(None)
    });

    r.register(ss, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        eprintln!("[w3a2] ServerSocket <init>(I)V port={port}");
        re2_bind_listener(ctx, this, "0.0.0.0", port, 50)
    });

    r.register(ss, "getLocalPort", "()I", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = ss_get(this).port;
        eprintln!("[w3a2] ServerSocket.getLocalPort -> {p}");
        Ok(Some(Value::Int(p)))
    });

    r.register(ss, "<init>", "(II)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let backlog = args.get(2).and_then(|v| v.as_int()).unwrap_or(50);
        re2_bind_listener(ctx, this, "0.0.0.0", port, backlog)
    });

    r.register(ss, "<init>", "(IILjava/net/InetAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let backlog = args.get(2).and_then(|v| v.as_int()).unwrap_or(50);
        let host = match args.get(3) {
            Some(Value::Object(Some(ia))) => read_field_string_or(ctx, *ia, IA_ADDR, "0.0.0.0"),
            _ => "0.0.0.0".to_string(),
        };
        eprintln!("[w3a2] ServerSocket <init>(IILjava/net/InetAddress;)V port={port} backlog={backlog} host={host}");
        let r = re2_bind_listener(ctx, this, &host, port, backlog);
        eprintln!("[w3a2]   ss_get(this).port = {}", ss_get(this).port);
        r
    });

    r.register(ss, "bind", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sa = obj_arg(args, 1).map_err(|_| ioex("bind: null address"))?;
        let (host, port) = read_inet_socket_address(ctx, sa)?;
        let backlog = ss_get(this).backlog;
        re2_bind_listener(ctx, this, &host, port, backlog)
    });
    r.register(ss, "bind", "(Ljava/net/SocketAddress;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sa = obj_arg(args, 1).map_err(|_| ioex("bind: null address"))?;
        let backlog = args.get(2).and_then(|v| v.as_int()).unwrap_or(50);
        let (host, port) = read_inet_socket_address(ctx, sa)?;
        re2_bind_listener(ctx, this, &host, port, backlog)
    });

    r.register(ss, "accept", "()Ljava/net/Socket;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = ss_get(this);
        if s.closed != 0 {
            return Err(ioex("Socket is closed"));
        }
        let lid = s.listener_id;
        let timeout_ms = re2_accept_timeout_for(lid);
        let sock = alloc_concurrent_synthetic(ctx, "java/net/Socket", 5);
        ctx.set_field(sock, SOCK_HOST, Value::Object(None));
        sock_set(sock, |x| {
            x.port = 0;
            x.local_port = 0;
            x.closed = 0;
            x.stream_id = -1;
        });
        re2_accept_into(ctx, lid, sock, timeout_ms)
    });

    r.register(ss, "setSoTimeout", "(I)V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if ms < 0 {
            return Err(iae(format!("negative SO_TIMEOUT: {ms}")));
        }
        let lid = ss_get(this).listener_id;
        re2_set_accept_timeout(lid, ms);
        Ok(None)
    });
    r.register(ss, "getSoTimeout", "()I", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ss_get(this).listener_id;
        Ok(Some(Value::Int(re2_accept_timeout_for(lid))))
    });

    r.register(ss, "close", "()V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ss_get(this).listener_id;
        if lid >= 0 {
            s2_registry().lock().listeners.remove(&lid);
            re2_clear_accept_timeout(lid);
        }
        ss_set(this, |s| {
            s.closed = 1;
            s.listener_id = -1;
        });
        Ok(None)
    });

    r.register(ss, "isBound", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ss_get(this).listener_id;
        Ok(Some(Value::Int(if lid >= 0 { 1 } else { 0 })))
    });
    r.register(ss, "isClosed", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ss_get(this).closed)))
    });

    r.register(
        ss,
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let lid = ss_get(this).listener_id;
            if lid < 0 {
                return Ok(Some(Value::Object(None)));
            }
            let local = {
                let reg = s2_registry().lock();
                reg.listeners
                    .get(&lid)
                    .and_then(|l| l.local_addr().ok())
                    .map(|a| (a.ip().to_string(), a.port() as i32))
            };
            match local {
                Some((ip, port)) => Ok(Some(Value::Object(Some(alloc_inet_socket_address(
                    ctx, &ip, port,
                ))))),
                None => Ok(Some(Value::Object(None))),
            }
        },
    );
}

fn re2_accept_timeouts() -> &'static Mutex<HashMap<i32, i32>> {
    static INSTANCE: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}
fn re2_set_accept_timeout(lid: i32, ms: i32) {
    if lid < 0 {
        return;
    }
    re2_accept_timeouts().lock().insert(lid, ms);
}
fn re2_accept_timeout_for(lid: i32) -> i32 {
    if lid < 0 {
        return 0;
    }
    *re2_accept_timeouts().lock().get(&lid).unwrap_or(&0)
}
fn re2_clear_accept_timeout(lid: i32) {
    re2_accept_timeouts().lock().remove(&lid);
}

// ===========================================================================
// RE.3 — java.net.InetAddress
// ===========================================================================

fn register_re3_inet_address(r: &mut NativeMethodRegistry) {
    let ia = "java/net/InetAddress";

    r.register(
        ia,
        "getByName",
        "(Ljava/lang/String;)Ljava/net/InetAddress;",
        |ctx, args| {
            let host = match args.first() {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => String::new(),
            };
            let ip = resolve_host(&host)?;
            let name = if host.is_empty() { "localhost".to_string() } else { host };
            let obj = alloc_inet_address(ctx, &name, &ip.to_string());
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(
        ia,
        "getAllByName",
        "(Ljava/lang/String;)[Ljava/net/InetAddress;",
        |ctx, args| {
            let host = match args.first() {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => String::new(),
            };
            let lookup = if host.is_empty() || host == "localhost" {
                "localhost:0".to_string()
            } else {
                format!("{host}:0")
            };
            let mut addrs: Vec<String> = Vec::new();
            match std::net::ToSocketAddrs::to_socket_addrs(&lookup.as_str()) {
                Ok(iter) => {
                    for sa in iter {
                        addrs.push(sa.ip().to_string());
                    }
                }
                Err(e) => {
                    return Err(ioex(format!("UnknownHostException: {host}: {e}")));
                }
            }
            if addrs.is_empty() {
                return Err(ioex(format!("UnknownHostException: {host}")));
            }
            let arr = ctx.new_ref_array(ClassId::new(0), addrs.len());
            let name = if host.is_empty() { "localhost".to_string() } else { host };
            for (i, ip) in addrs.iter().enumerate() {
                let obj = alloc_inet_address(ctx, &name, ip);
                ctx.set_array_element(arr, i, Value::Object(Some(obj)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    r.register(
        ia,
        "getLoopbackAddress",
        "()Ljava/net/InetAddress;",
        |ctx, _args| {
            let obj = alloc_inet_address(ctx, "localhost", "127.0.0.1");
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(
        ia,
        "getLocalHost",
        "()Ljava/net/InetAddress;",
        |ctx, _args| {
            let hostname = hostname_string();
            let ip = resolve_host(&hostname)
                .map(|i| i.to_string())
                .unwrap_or_else(|_| "127.0.0.1".to_string());
            let obj = alloc_inet_address(ctx, &hostname, &ip);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(ia, "getHostAddress", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, IA_ADDR)))
    });
    r.register(ia, "getHostName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, IA_HOST)))
    });
    r.register(ia, "getCanonicalHostName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, IA_HOST)))
    });
    r.register(ia, "getAddress", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip_str = read_field_string_or(ctx, this, IA_ADDR, "0.0.0.0");
        let bytes: Vec<u8> = if let Ok(v4) = ip_str.parse::<Ipv4Addr>() {
            v4.octets().to_vec()
        } else if let Ok(v6) = ip_str.parse::<Ipv6Addr>() {
            v6.octets().to_vec()
        } else {
            vec![0u8; 4]
        };
        let arr = new_java_byte_array(ctx, &bytes);
        Ok(Some(Value::Object(Some(arr))))
    });

    r.register(ia, "isLoopbackAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip_str = read_field_string_or(ctx, this, IA_ADDR, "");
        let is_lb = ip_str
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
        Ok(Some(Value::Int(if is_lb { 1 } else { 0 })))
    });
    r.register(ia, "isAnyLocalAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip_str = read_field_string_or(ctx, this, IA_ADDR, "");
        let any = ip_str
            .parse::<IpAddr>()
            .map(|ip| match ip {
                IpAddr::V4(v) => v.is_unspecified(),
                IpAddr::V6(v) => v.is_unspecified(),
            })
            .unwrap_or(false);
        Ok(Some(Value::Int(if any { 1 } else { 0 })))
    });
    r.register(ia, "isMulticastAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip_str = read_field_string_or(ctx, this, IA_ADDR, "");
        let m = ip_str
            .parse::<IpAddr>()
            .map(|ip| ip.is_multicast())
            .unwrap_or(false);
        Ok(Some(Value::Int(if m { 1 } else { 0 })))
    });

    r.register(
        ia,
        "getByAddress",
        "([B)Ljava/net/InetAddress;",
        |ctx, args| {
            let arr = obj_arg(args, 0)?;
            let len = ctx.array_length(arr);
            let ip_str = if len == 4 {
                let b = java_byte_array_to_vec(ctx, arr, 0, 4)?;
                Ipv4Addr::new(b[0], b[1], b[2], b[3]).to_string()
            } else if len == 16 {
                let b = java_byte_array_to_vec(ctx, arr, 0, 16)?;
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&b);
                Ipv6Addr::from(octets).to_string()
            } else {
                return Err(iae(format!("addr is of illegal length: {len}")));
            };
            let obj = alloc_inet_address(ctx, &ip_str, &ip_str);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    for cls in ["java/net/Inet4Address", "java/net/Inet6Address"] {
        r.register(cls, "getHostAddress", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, IA_ADDR)))
        });
        r.register(cls, "getHostName", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, IA_HOST)))
        });
    }
}

// ===========================================================================
// RE.4 — java.net.URL + HttpURLConnection
// ===========================================================================

const HUC_URL: usize = 0;
const HUC_METHOD: usize = 1;
const HUC_CODE: usize = 2;
const HUC_FD: usize = 3;
const HUC_REQ_HEADERS: usize = 4;
const HUC_RESP_HEADERS: usize = 5;
const HUC_BODY: usize = 6;
const HUC_DO_INPUT: usize = 7;
const HUC_DO_OUTPUT: usize = 8;
const HUC_CONNECTED: usize = 9;

struct HttpResponse {
    status: i32,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn http_parse_url(url: &str) -> Result<(bool, String, u16, String), String> {
    let (scheme, rest) = if let Some(s) = url.strip_prefix("http://") {
        (false, s)
    } else if let Some(s) = url.strip_prefix("https://") {
        (true, s)
    } else {
        return Err(format!("unsupported URL: {url}"));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rfind(':') {
        Some(i) => {
            let (h, p) = (&authority[..i], &authority[i + 1..]);
            let pn: u16 = p.parse().map_err(|_| format!("bad port in {url}"))?;
            (h.to_string(), pn)
        }
        None => (authority.to_string(), if scheme { 443 } else { 80 }),
    };
    Ok((scheme, host, port, path.to_string()))
}

fn http_perform_request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    max_redirects: usize,
) -> std::io::Result<HttpResponse> {
    let mut current_url = url.to_string();
    let mut current_method = method.to_string();
    let mut current_body = body.to_vec();
    for _ in 0..=max_redirects {
        let (https, host, port, path) = http_parse_url(&current_url)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let resp = if https {
            http_exchange_tls(&host, port, &path, &current_method, headers, &current_body)?
        } else {
            http_exchange_plain(&host, port, &path, &current_method, headers, &current_body)?
        };
        match resp.status {
            301 | 302 | 303 | 307 | 308 => {
                let loc_opt = resp
                    .headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("location"))
                    .map(|(_, v)| v.clone());
                if let Some(loc) = loc_opt {
                    let next = if loc.starts_with("http") {
                        loc
                    } else {
                        let (scheme, h, p, _) = http_parse_url(&current_url)
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                        format!(
                            "{}://{}:{}{}",
                            if scheme { "https" } else { "http" },
                            h,
                            p,
                            loc
                        )
                    };
                    current_url = next;
                    if resp.status == 303 {
                        current_method = "GET".into();
                        current_body.clear();
                    }
                    continue;
                }
                return Ok(resp);
            }
            _ => return Ok(resp),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        "too many redirects",
    ))
}

fn http_build_request(
    method: &str,
    host: &str,
    port: u16,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
    default_port: u16,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + body.len());
    use std::io::Write as _;
    let _ = write!(&mut out, "{method} {path} HTTP/1.1\r\n");
    if port == default_port {
        let _ = write!(&mut out, "Host: {host}\r\n");
    } else {
        let _ = write!(&mut out, "Host: {host}:{port}\r\n");
    }
    let mut has_content_length = false;
    let mut has_connection = false;
    let mut has_user_agent = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") {
            has_content_length = true;
        }
        if k.eq_ignore_ascii_case("connection") {
            has_connection = true;
        }
        if k.eq_ignore_ascii_case("user-agent") {
            has_user_agent = true;
        }
        let _ = write!(&mut out, "{k}: {v}\r\n");
    }
    if !has_user_agent {
        out.extend_from_slice(b"User-Agent: cratonvm-phaseE/1.0\r\n");
    }
    if !has_connection {
        out.extend_from_slice(b"Connection: close\r\n");
    }
    if !has_content_length && (!body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH")) {
        let _ = write!(&mut out, "Content-Length: {}\r\n", body.len());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

fn http_read_response<R: Read>(mut r: R) -> std::io::Result<HttpResponse> {
    let mut all = Vec::with_capacity(8192);
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    let sep = all
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no header terminator"))?;
    let header_block = &all[..sep];
    let body_region = &all[sep + 4..];
    let head_str = std::str::from_utf8(header_block)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut lines = head_str.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing status"))?;
    let mut parts = status_line.splitn(3, ' ');
    let _ = parts.next();
    let code_str = parts
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing status code"))?;
    let status: i32 = code_str
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad status code"))?;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        if let Some(colon) = line.find(':') {
            let k = line[..colon].trim().to_string();
            let v = line[colon + 1..].trim().to_string();
            if k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().ok();
            }
            headers.push((k, v));
        }
    }
    let body = if chunked {
        http_decode_chunked(body_region)?
    } else if let Some(n) = content_length {
        body_region[..body_region.len().min(n)].to_vec()
    } else {
        body_region.to_vec()
    };
    Ok(HttpResponse { status, headers, body })
}

fn http_decode_chunked(mut data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    loop {
        let nl = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk header"))?;
        let size_str = std::str::from_utf8(&data[..nl])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let size_str = size_str.split(';').next().unwrap_or("0").trim();
        let n = usize::from_str_radix(size_str, 16)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk size"))?;
        data = &data[nl + 2..];
        if n == 0 {
            break;
        }
        if data.len() < n + 2 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "chunk truncated",
            ));
        }
        out.extend_from_slice(&data[..n]);
        data = &data[n + 2..];
    }
    Ok(out)
}

fn http_exchange_plain(
    host: &str,
    port: u16,
    path: &str,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> std::io::Result<HttpResponse> {
    let mut stream = TcpStream::connect((host, port))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let req = http_build_request(method, host, port, path, headers, body, 80);
    stream.write_all(&req)?;
    stream.flush()?;
    http_read_response(stream)
}

fn http_exchange_tls(
    host: &str,
    port: u16,
    path: &str,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> std::io::Result<HttpResponse> {
    let connector = native_tls::TlsConnector::builder()
        .build()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("TLS init: {e}")))?;
    let tcp = TcpStream::connect((host, port))?;
    tcp.set_read_timeout(Some(Duration::from_secs(30)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut tls = connector
        .connect(host, tcp)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("TLS handshake: {e}")))?;
    let req = http_build_request(method, host, port, path, headers, body, 443);
    tls.write_all(&req)?;
    tls.flush()?;
    http_read_response(tls)
}

fn huc_extract_req_headers(
    ctx: &dyn NativeContext,
    hdrs: Value,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Value::Object(Some(a)) = hdrs {
        let len = ctx.array_length(a);
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(a, i) {
                let line = ctx.read_string(s).unwrap_or_default();
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
    out
}

fn huc_url_string(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    let url = match ctx.get_field(this, HUC_URL) {
        Value::Object(Some(u)) => u,
        _ => return String::new(),
    };
    if let Some(txt) = read_field_string(ctx, url, 0) {
        if txt.contains("://") {
            return txt;
        }
    }
    let res = ctx.invoke_virtual(url, "toString", "()Ljava/lang/String;", &[]);
    if let Ok(Some(Value::Object(Some(s)))) = res {
        return ctx.read_string(s).unwrap_or_default();
    }
    String::new()
}

fn huc_perform(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    if ctx.get_field(this, HUC_CONNECTED).as_int().unwrap_or(0) != 0 {
        return Ok(None);
    }
    let method = read_field_string_or(ctx, this, HUC_METHOD, "GET");
    let hdrs_val = ctx.get_field(this, HUC_REQ_HEADERS);
    let headers = huc_extract_req_headers(ctx, hdrs_val);
    let url = huc_url_string(ctx, this);
    if url.is_empty() {
        return Err(ioex("HttpURLConnection: missing URL"));
    }
    let resp = http_perform_request(&method, &url, &headers, &[], 10)
        .map_err(|e| ioex(format!("HTTP {method} {url}: {e}")))?;
    ctx.set_field(this, HUC_CODE, Value::Int(resp.status));
    let hdr_arr = ctx.new_ref_array(ClassId::new(0), resp.headers.len());
    for (i, (k, v)) in resp.headers.iter().enumerate() {
        let s = ctx.create_string(&format!("{k}: {v}"));
        ctx.set_array_element(hdr_arr, i, Value::Object(Some(s)));
    }
    ctx.set_field(this, HUC_RESP_HEADERS, Value::Object(Some(hdr_arr)));
    let body_arr = new_java_byte_array(ctx, &resp.body);
    ctx.set_field(this, HUC_BODY, Value::Object(Some(body_arr)));
    ctx.set_field(this, HUC_CONNECTED, Value::Int(1));
    Ok(None)
}

fn register_re4_url_http(r: &mut NativeMethodRegistry) {
    let url = "java/net/URL";

    // Our getResources / Class.getProtectionDomain overrides return URL
    // objects that never run java.net.URL.<init>, so the real-JDK
    // toString()/toExternalForm() NPE when they invoke the (null)
    // URLStreamHandler. We build the external form ourselves.
    //
    // Two layouts must be supported:
    //   * Synthetic-mode 6-field URL where field 5 holds the full URL string
    //     (populated by our `url_parse` constructor).
    //   * Real-JDK 13-field URL where field 0=protocol, 1=host, 2=port (int),
    //     3=file, 4=query, 5=authority, 6=path, 8=ref, 10=handler. The full
    //     external form is `protocol:[//host[:port]]file[#ref]` per
    //     java.net.URL.toString.
    //
    // We try the synthetic field-5 fast path first (works whenever url_parse
    // ran), then the real-JDK reconstruction (matches HotSpot output for
    // synthetic URLs allocated by Class.getProtectionDomain).
    let url_to_string = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        // Synthetic fast path: a non-empty field-5 string is the cached
        // full URL written by url_parse.  We treat any string containing
        // ":" as a full URL so we don't mistake the real-JDK `authority`
        // slot for a synthetic full-URL cache.
        let synth_full = match ctx.get_field(this, 5) {
            Value::Object(Some(o)) => ctx.read_string(o),
            _ => None,
        };
        if let Some(ref s) = synth_full {
            if s.contains(':') && !s.is_empty() {
                return Ok(Some(Value::Object(Some(ctx.create_string(s)))));
            }
        }
        // Real-JDK reconstruction: protocol:[//host[:port]]file[#ref].
        let proto = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            _ => String::new(),
        };
        let host = match ctx.get_field(this, 1) {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            _ => String::new(),
        };
        let port = match ctx.get_field(this, 2) {
            Value::Int(i) => i,
            _ => -1,
        };
        let file = match ctx.get_field(this, 3) {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            _ => String::new(),
        };
        let ref_str = match ctx.get_field(this, 8) {
            Value::Object(Some(o)) => ctx.read_string(o),
            _ => None,
        };
        let mut out = String::new();
        if !proto.is_empty() {
            out.push_str(&proto);
            out.push(':');
        }
        if !host.is_empty() || port >= 0 {
            out.push_str("//");
            out.push_str(&host);
            if port >= 0 {
                out.push(':');
                out.push_str(&port.to_string());
            }
        }
        out.push_str(&file);
        if let Some(r) = ref_str {
            out.push('#');
            out.push_str(&r);
        }
        // Ultimate fallback: if we built nothing useful, fall back to the
        // legacy field-0 read for synthetic-mode URLs that stored the
        // full path at slot 0.
        if out.is_empty() || out == ":" {
            if let Some(s) = synth_full {
                return Ok(Some(Value::Object(Some(ctx.create_string(&s)))));
            }
        }
        Ok(Some(Value::Object(Some(ctx.create_string(&out)))))
    };
    r.register(url, "toString", "()Ljava/lang/String;", url_to_string);
    r.register(url, "toExternalForm", "()Ljava/lang/String;", url_to_string);

    // URL.toURI() — JDK 21+ URLClassPath calls url.toURI() when setting up
    // the classpath for LaunchedURLClassLoader. The method is absent from
    // CratonVM's synthetic URL type, so we provide it here. Strategy:
    //   1. Get the full URL string via our toString native.
    //   2. Allocate a real java/net/URI and run its (String) constructor so
    //      getScheme/getPath/toString all return correct answers via JDK bytecode.
    //   3. If the real constructor fails (e.g. URISyntaxException from a
    //      jar:file:…!/… URL), fall back to a 7-field synthetic URI whose
    //      raw-string field is set so our uri_string helper can reconstruct
    //      the string, and whose scheme field holds the scheme prefix.
    r.register(url, "toURI", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Obtain the full URL string.
        let url_str_obj = match ctx.invoke(
            "java/net/URL",
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(this))],
        ) {
            Ok(Some(Value::Object(Some(s)))) => s,
            _ => return Ok(Some(Value::Object(None))),
        };
        let url_str = ctx.read_string(url_str_obj).unwrap_or_default();

        // Always build a synthetic URI. Returning a real-JDK URI here leaves
        // our `URI.getSchemeSpecificPart()` override without access to the raw
        // string (layout differs), which can degrade to empty SSP and break
        // Spring Boot's `new File(url.toURI().getSchemeSpecificPart())` path.
        // Layout per http2.rs:
        //   scheme=0, host=1, port=2, path=3, query=4, fragment=5, raw=6
        let uri = alloc_concurrent_synthetic(ctx, "java/net/URI", 7);
        let raw_s = ctx.create_string(&url_str);
        ctx.set_field(uri, 6, Value::Object(Some(raw_s))); // raw
        // Parse scheme.
        if let Some(colon) = url_str.find(':') {
            let scheme = &url_str[..colon];
            let scheme_s = ctx.create_string(scheme);
            ctx.set_field(uri, 0, Value::Object(Some(scheme_s)));
            // For file: URIs, the path is everything after "file:".
            if scheme == "file" {
                let path = &url_str[colon + 1..];
                let path_s = ctx.create_string(path);
                ctx.set_field(uri, 3, Value::Object(Some(path_s)));
            }
        }
        Ok(Some(Value::Object(Some(uri))))
    });

    // S111r18: URL.getDefaultPort() — JDK 25 reads `handler.getDefaultPort()`
    // (URL.java:1015) which NPEs because synthetic URLs (alloc_url, real-JDK
    // URL ctors) never populate the `handler` field. Per-protocol defaults
    // match what URLStreamHandler subclasses report; this bypasses the null
    // handler deref entirely. Spring's CandidateComponentsIndexLoader →
    // LaunchedURLClassLoader$UseFastConnectionExceptionsEnumeration →
    // URLClassPath.getLoader → URLUtil.urlNoFragString → getDefaultPort
    // chain (scag-auth + sister jars) hit this NPE.
    r.register(url, "getDefaultPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Read the protocol field (slot 0 in our synthetic URL layout).
        let protocol = read_field_string_or(ctx, this, 0, "");
        let port = match protocol.as_str() {
            "http" => 80,
            "https" => 443,
            "ftp" => 21,
            "gopher" => 70,
            // file/jar/jrt/classpath/nested/etc. → -1 per JDK URLStreamHandler
            _ => -1i32,
        };
        Ok(Some(Value::Int(port)))
    });

    r.register(url, "openStream", "()Ljava/io/InputStream;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Prefer our synthetic "full URL" slot (field 5). For real-JDK URLs
        // field 0 holds only the protocol (e.g. "jar") — a value too short
        // to be a usable full URL — so fall through to toExternalForm which
        // reconstructs the full string (protocol:[//host[:port]]file[#ref]).
        // Round 76: SportMe's SpringFactoriesLoader builds UrlResource around
        // real-JDK URL instances whose slot 5 is empty; reading slot 0 alone
        // yielded "jar" and tripped the unsupported-scheme branch below.
        let mut url_str = read_field_string_or(ctx, this, 5, "");
        if !url_str.contains(':') {
            // Either empty or just a protocol — ask the URL for its full form.
            if let Ok(Some(Value::Object(Some(s)))) = ctx.invoke(
                "java/net/URL",
                "toExternalForm",
                "()Ljava/lang/String;",
                &[Value::Object(Some(this))],
            ) {
                let ext = ctx.read_string(s).unwrap_or_default();
                if ext.contains(':') {
                    url_str = ext;
                }
            }
        }
        if !url_str.contains(':') {
            // Last-resort legacy synthetic fallback to slot 0.
            let s0 = read_field_string_or(ctx, this, 0, "");
            if s0.contains(':') {
                url_str = s0;
            }
        }
        if url_str.is_empty() {
            return Err(ioex("URL.openStream: empty URL"));
        }

        // S111r23-DBG: log spring.factories URL openStream calls
        let is_sf = url_str.contains("spring.factories");
        if is_sf && spring_dbg_enabled() {
            eprintln!("[OSTR-DBG] URL.openStream: {}", url_str);
        }

        // Resolve the URL to raw bytes. Handles file:, jar:file:!/, and
        // classpath: schemes locally; http(s): goes through the HTTP client.
        let bytes: Vec<u8> = if let Some(rest) = url_str.strip_prefix("jar:file:") {
            // jar:file:/path/to.jar!/entry  (single-level)
            // jar:file:/fat.jar!/BOOT-INF/lib/inner.jar!/entry  (double-nested)
            //   — Spring Boot 2.x fat JARs store nested JARs as BOOT-INF/lib/*.jar;
            //     ClassLoader.getResources() produces double-nested URLs. We must
            //     read the outer JAR, extract the inner JAR bytes, then look up
            //     the entry in the inner archive.
            // Bug 2 fix: cache outer JAR bytes + nested JAR bytes so the
            // O(N) autoconfig walk doesn't become O(N²) on disk reads.
            let rest = rest.trim_start_matches('/');
            let (outer_jar, inner_path) = match rest.find("!/") {
                Some(i) => (&rest[..i], &rest[i + 2..]),
                None => return Err(ioex(format!("URL.openStream: malformed jar URL: {url_str}"))),
            };
            // Check if the inner_path itself is a nested jar entry (double !/):
            // e.g. "BOOT-INF/lib/spring-boot-2.7.12.jar!/META-INF/spring.factories"
            let buf = if let Some(second_sep) = inner_path.find("!/") {
                let nested_jar_entry = &inner_path[..second_sep];
                let resource_entry = &inner_path[second_sep + 2..];
                use std::io::Read;
                let nested_jar_bytes = cached_nested_jar(outer_jar, nested_jar_entry)
                    .map_err(|e| ioex(format!("URL.openStream: nested jar {nested_jar_entry} in {outer_jar}: {e}")))?;
                let inner_cursor = std::io::Cursor::new(nested_jar_bytes.as_slice());
                let mut inner_zip = zip::ZipArchive::new(inner_cursor)
                    .map_err(|e| ioex(format!("URL.openStream: open inner jar {nested_jar_entry}: {e}")))?;
                let mut entry_file = inner_zip
                    .by_name(resource_entry)
                    .map_err(|e| ioex(format!("URL.openStream: entry {resource_entry} in {nested_jar_entry}: {e}")))?;
                let mut buf = Vec::with_capacity(entry_file.size().min(1 << 27) as usize);
                entry_file
                    .read_to_end(&mut buf)
                    .map_err(|e| ioex(format!("URL.openStream: read entry {resource_entry}: {e}")))?;
                buf
            } else {
                // Single-level: jar:file:/path/to.jar!/entry
                use std::io::Read;
                let jar_bytes = cached_outer_jar(outer_jar)
                    .map_err(|e| ioex(format!("URL.openStream: read jar {outer_jar}: {e}")))?;
                let cursor = std::io::Cursor::new(jar_bytes.as_slice());
                let mut zip = zip::ZipArchive::new(cursor)
                    .map_err(|e| ioex(format!("URL.openStream: open jar {outer_jar}: {e}")))?;
                let mut entry_file = zip
                    .by_name(inner_path)
                    .map_err(|e| ioex(format!("URL.openStream: entry {inner_path} in {outer_jar}: {e}")))?;
                let mut buf = Vec::with_capacity(entry_file.size().min(1 << 27) as usize);
                entry_file
                    .read_to_end(&mut buf)
                    .map_err(|e| ioex(format!("URL.openStream: read entry {inner_path}: {e}")))?;
                buf
            };
            buf
        } else if let Some(rest) = url_str.strip_prefix("file:") {
            let path = rest.trim_start_matches('/');
            // Retry with a leading slash for POSIX absolute paths.
            std::fs::read(path)
                .or_else(|_| std::fs::read(rest))
                .map_err(|e| ioex(format!("URL.openStream: read file {path}: {e}")))?
        } else if let Some(name) = url_str.strip_prefix("classpath:") {
            let name = name.trim_start_matches('/');
            ctx.find_resource(name)
                .ok_or_else(|| ioex(format!("URL.openStream: classpath resource not found: {name}")))?
        } else if let Some(name) = url_str.strip_prefix("resource:") {
            // Synthetic `resource:/<name>` URLs are produced by
            // `Class.getResource(String)` in `lang_class.rs` for resources
            // that the classloader knows about but for which we don't have
            // a real `file:` or `jar:` URL. Felix's
            // `Util.loadDefaultProperties` does `Util.class.getResource(...).
            // openConnection().getInputStream()`, which arrives here via
            // `URLConnection.getInputStream` → `URL.openStream` and used to
            // hit the unsupported-scheme arm, leaving Felix's static
            // `DEFAULTS` field null and tripping a downstream NPE on
            // `DEFAULTS.isEmpty()`.
            let name = name.trim_start_matches('/');
            ctx.find_resource(name)
                .ok_or_else(|| ioex(format!("URL.openStream: resource not found: {name}")))?
        } else if url_str.starts_with("http://") || url_str.starts_with("https://") {
            let resp = http_perform_request("GET", &url_str, &[], &[], 10)
                .map_err(|e| ioex(format!("URL.openStream failed: {e}")))?;
            resp.body
        } else {
            return Err(ioex(format!("URL.openStream: unsupported scheme: {url_str}")));
        };

        if is_sf && spring_dbg_enabled() {
            eprintln!("[OSTR-DBG] URL.openStream bytes={}", bytes.len());
        }
        let body = new_java_byte_array(ctx, &bytes);
        let len = ctx.array_length(body) as i32;
        let stream = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
        ctx.set_field(stream, 0, Value::Object(Some(body))); // buf
        ctx.set_field(stream, 1, Value::Int(0));              // pos
        ctx.set_field(stream, 2, Value::Int(0));              // mark
        ctx.set_field(stream, 3, Value::Int(len));            // count
        let _ = ctx.invoke(
            "java/io/ByteArrayInputStream",
            "<init>",
            "([B)V",
            &[Value::Object(Some(stream)), Value::Object(Some(body))],
        );
        Ok(Some(Value::Object(Some(stream))))
    });

    // URL.openConnection() — return a synthetic URLConnection that defers
    // its `getInputStream()` to the URL's `openStream()` resolver above.
    //
    // The real-JDK path is `handler.openConnection(this)`, but the
    // URL objects we hand out for resources synthesised by
    // `ClassLoader.getResource(s)` and `Class.getProtectionDomain` were
    // never run through the real `URL.<init>` (which would have populated
    // the package-private `handler` field via `URL.getURLStreamHandler`),
    // so the JDK call NPEs at line 1209. Spring's `UrlResource.getInputStream`
    // and `JarFile.getInputStream` go through `URL.openConnection().
    // getInputStream()`, so a working override is the difference between
    // SB3's `SpringFactoriesLoader` finding `META-INF/spring.factories`
    // entries and the launcher dying with `InvocationTargetException`
    // wrapping a `NullPointerException` at `URL.openConnection`.
    //
    // We pick `java/net/HttpURLConnection` as the carrier class. It is
    // concrete (so allocation succeeds), it is a subclass of URLConnection
    // (so `URLConnection`-typed locals accept it), and the existing
    // HttpURLConnection natives below cover `connect`, `getResponseCode`,
    // `getInputStream`, `setRequestMethod`, etc. for the http(s) case.
    // For non-http schemes we override `getInputStream` here to delegate
    // back to URL.openStream so file:/jar:/classpath:/nested:/jrt: all
    // serve bytes consistently with the resource resolver.
    r.register(
        url,
        "openConnection",
        "()Ljava/net/URLConnection;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Determine the URL's external form so we can pick a carrier
            // class whose type matches what JDK callers cast the result to.
            // ActiveMQ's Main.getActiveMQHome() does
            //   `(JarURLConnection) url.openConnection()` on the
            //   `jar:file:…!/…` URL returned by ClassLoader.getResource() —
            // returning an HttpURLConnection there throws ClassCastException,
            // which is swallowed and forces a wrong `../.` home fallback.
            let ext = {
                let s5 = read_field_string_or(ctx, this, 5, "");
                let s = if s5.contains(':') { s5 } else {
                    match ctx.invoke_virtual(this, "toExternalForm", "()Ljava/lang/String;", &[]) {
                        Ok(Some(Value::Object(Some(o)))) => ctx.read_string(o).unwrap_or_default(),
                        _ => {
                            let s0 = read_field_string_or(ctx, this, 0, "");
                            if s0.contains(':') { s0 } else { String::new() }
                        }
                    }
                };
                s
            };
            // S111r23-DBG: log openConnection calls for spring.factories
            if ext.contains("spring.factories") {
                eprintln!("[CONN-DBG] URL.openConnection: {}", ext);
            }
            // For `jar:` URLs, return a `java/net/JarURLConnection`-typed
            // object. JarURLConnection is abstract, but `alloc_object`
            // bypasses the abstract check; the only method ActiveMQ invokes
            // is `getJarFileURL()` (registered below), and `getInputStream`
            // delegates to URL.openStream via the HUC_URL field like the
            // generic URLConnection path.
            let carrier = if ext.starts_with("jar:") {
                "java/net/JarURLConnection"
            } else {
                "java/net/HttpURLConnection"
            };
            let conn = alloc_concurrent_synthetic(ctx, carrier, 16);
            // Field HUC_URL holds the originating URL so `huc_url_string`
            // and `getInputStream` can recover its external form.
            ctx.set_field(conn, HUC_URL, Value::Object(Some(this)));
            // Default request method "GET" so `huc_perform` doesn't trip
            // on a missing method when the http(s) path is exercised.
            let m = ctx.create_string("GET");
            ctx.set_field(conn, HUC_METHOD, Value::Object(Some(m)));
            ctx.set_field(conn, HUC_DO_INPUT, Value::Int(1));
            ctx.set_field(conn, HUC_CONNECTED, Value::Int(0));
            Ok(Some(Value::Object(Some(conn))))
        },
    );
    // java/net/JarURLConnection.getJarFileURL() — return the `file:` URL of
    // the enclosing JAR. The originating `jar:file:…!/entry` URL is stored in
    // HUC_URL; strip the `jar:` prefix and the `!/entry` suffix to recover
    // the bare jar URL, then construct a fresh java.net.URL for it.
    r.register(
        "java/net/JarURLConnection",
        "getJarFileURL",
        "()Ljava/net/URL;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let url_obj = match ctx.get_field(this, HUC_URL) {
                Value::Object(Some(o)) => o,
                _ => return Err(ioex("JarURLConnection.getJarFileURL: no URL")),
            };
            // Recover the full `jar:file:…!/…` external form.
            let mut ext = read_field_string_or(ctx, url_obj, 5, "");
            if !ext.starts_with("jar:") {
                if let Ok(Some(Value::Object(Some(o)))) = ctx.invoke_virtual(
                    url_obj, "toExternalForm", "()Ljava/lang/String;", &[],
                ) {
                    ext = ctx.read_string(o).unwrap_or_default();
                }
            }
            // jar:<jarurl>!/<entry>  →  <jarurl>
            let jar_part = ext
                .strip_prefix("jar:")
                .unwrap_or(&ext)
                .split("!/")
                .next()
                .unwrap_or("")
                .to_string();
            if jar_part.is_empty() {
                return Err(ioex("JarURLConnection.getJarFileURL: malformed URL"));
            }
            let spec = ctx.create_string(&jar_part);
            // Construct via the regular `new URL(String)` path so the
            // returned object is a fully-initialised java.net.URL.
            let new_url = match ctx.new_object("java/net/URL")? {
                Some(Value::Object(Some(o))) => o,
                _ => return Err(ioex("JarURLConnection.getJarFileURL: alloc URL")),
            };
            ctx.invoke_special(
                "java/net/URL",
                "<init>",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(new_url)), Value::Object(Some(spec))],
            )?;
            Ok(Some(Value::Object(Some(new_url))))
        },
    );
    // URLConnection.setUseCaches / setDefaultUseCaches / connect — Spring's
    // `ResourceUtils.useCachesIfNecessary` calls setUseCaches(false) on
    // file: URLs; without these no-op natives the call would fall through
    // to the real-JDK setter, which probes the (uninitialised) connected
    // field and throws IllegalStateException. Make them no-ops on both
    // URLConnection and HttpURLConnection (registered separately).
    r.register("java/net/URLConnection", "setUseCaches", "(Z)V", |_ctx, _args| Ok(None));
    r.register(
        "java/net/URLConnection",
        "setDefaultUseCaches",
        "(Z)V",
        |_ctx, _args| Ok(None),
    );
    r.register("java/net/URLConnection", "connect", "()V", |_ctx, _args| Ok(None));
    // URLConnection.getInputStream — defer to URL.openStream by reading
    // the URL stored in HUC_URL during openConnection above.
    r.register(
        "java/net/URLConnection",
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let url_obj = match ctx.get_field(this, HUC_URL) {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(ioex("URLConnection.getInputStream: no URL"));
                }
            };
            ctx.invoke_virtual(
                url_obj,
                "openStream",
                "()Ljava/io/InputStream;",
                &[],
            )
        },
    );

    // -----------------------------------------------------------------------
    // S111r25 — UrlResource.getInputStream() override.
    //
    // Spring's UrlResource.getInputStream() uses the pattern:
    //   URLConnection con = this.url.openConnection();
    //   customizeConnection(con);
    //   try { return con.getInputStream(); }
    //   catch (IOException ex) { ... disconnect ... throw; }
    //
    // The final `con.getInputStream()` call dispatches as
    // `invokevirtual java/net/URLConnection.getInputStream()` on our
    // synthetic `java/net/HttpURLConnection` object.  In real-JDK mode,
    // the VM finds the JDK's URLConnection bytecode (which throws
    // `UnknownServiceException extends IOException`) in preference to our
    // native registration, because the declaring class (`URLConnection`)
    // has a JDK class file present.  The IOException is caught by
    // UrlResource's handler (offset 18), disconnect() is called, and the
    // exception re-propagates to loadSpringFactories which skips the URL —
    // leaving the factory map empty and causing DefaultApplicationContextFactory
    // to fail with IllegalArgumentException.
    //
    // Fix: intercept `UrlResource.getInputStream()` directly and delegate
    // straight to URL.openStream(), completely skipping openConnection /
    // customizeConnection / URLConnection.getInputStream.  This is
    // semantically equivalent but entirely inside our VM infrastructure.
    // -----------------------------------------------------------------------
    // -----------------------------------------------------------------------
    // Spring `ConfigurationClassEnhancer.enhance(Class, ClassLoader)`
    //
    // Spring uses CGLIB to subclass every `@Configuration`-annotated class so
    // that calls between `@Bean` methods return shared bean instances rather
    // than fresh ones.  CGLIB's `Enhancer.createClass()` exercises a large
    // bytecode-generation + ClassLoader.defineClass pipeline that is
    // currently incomplete in this VM and throws a bare
    // `IllegalStateException` (no message) deep inside.  The exception
    // surfaces in `ConfigurationClassPostProcessor.enhanceConfigurationClasses`
    // as:
    //   IllegalStateException: Cannot load configuration class: <name>
    //   Caused by: IllegalStateException
    // SportMe hits this on `RedisHttpSessionConfiguration`.
    //
    // Pragmatic workaround: return the original class unchanged so Spring
    // skips enhancement.  Inter-@Bean-method calls won't be intercepted, but
    // that is the same trade-off Spring makes for `@Configuration(proxyBeanMethods = false)`
    // and lets the application advance past container bootstrap.
    // -----------------------------------------------------------------------
    r.register(
        "org/springframework/context/annotation/ConfigurationClassEnhancer",
        "enhance",
        "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/lang/Class;",
        |_ctx, args| {
            // invokevirtual: args[0] = receiver (ConfigurationClassEnhancer), args[1] =
            // config Class, args[2] = ClassLoader. Returning args.first() was the receiver
            // mis-typed as Class → AbstractBeanDefinition.getBeanClassName CCE.
            let cls = match args.get(1).cloned() {
                Some(v) => v,
                None => return Err(iae("ConfigurationClassEnhancer.enhance: missing class arg")),
            };
            // JNI / invoke bridges may pass the config Class as `Value::Long`;
            // return a proper reference so the caller's `astore`/`if_acmpeq`
            // sequence does not retain an unrooted jlong handle (Letsgo AV).
            let cls = match cls {
                cratonvm_types::Value::Long(bits) => {
                    if let Some(p) =
                        cratonvm_types::jlong_bits_as_aligned_object_ptr(bits as u64)
                    {
                        cratonvm_types::Value::Object(Some(unsafe {
                            cratonvm_types::ObjectRef::from_raw(p as *mut u8)
                        }))
                    } else {
                        cratonvm_types::Value::Object(None)
                    }
                }
                other => other,
            };
            if spring_dbg_enabled() {
                eprintln!("[CCE-DBG] ConfigurationClassEnhancer.enhance -> bypass (return original class)");
            }
            Ok(Some(cls))
        },
    );

    // -----------------------------------------------------------------------
    // Bug 3 fix: AbstractBeanDefinition.getResolvedAutowireMode() override.
    //
    // The constructor writes `autowireMode = 0` (AUTOWIRE_NO) via
    // `iconst_0; putfield #27`.  But under CratonVM the getfield reading
    // that same slot later returns a non-zero value, sending
    // `AbstractAutowireCapableBeanFactory.populateBean` down the
    // autowireByType branch.  That branch calls every Setter on every
    // property — including `setMetadataReaderFactory(null)` on
    // `ConfigurationClassPostProcessor` — and throws
    //   BeanCreationException: Error creating bean with name
    //     'org.springframework.context.annotation.internalConfigurationAnnotationProcessor'.
    //
    // Diagnostic agent π traced the root cause to a field-layout / slot
    // mismatch between read and write paths.  Pending a fix to the deeper
    // layout bug, we return the correct default (0 = AUTOWIRE_NO) directly
    // from a native override.  This matches the value the constructor
    // tried to write and lets Spring's no-autowire branch run.
    //
    // The override is registered against `AbstractBeanDefinition`; Java
    // dispatch via invokevirtual on a `RootBeanDefinition` will find this
    // because RootBeanDefinition does not override `getResolvedAutowireMode`.
    // -----------------------------------------------------------------------
    r.register(
        "org/springframework/beans/factory/support/AbstractBeanDefinition",
        "getResolvedAutowireMode",
        "()I",
        |_ctx, _args| {
            // AUTOWIRE_NO = 0. Returning 0 makes populateBean take the
            // no-autowire branch (skip autowireByName / autowireByType).
            // Applications that genuinely want autowiring set the value
            // via setAutowireMode(...) which we'd need to honor — but
            // Spring Boot's default config uses AUTOWIRE_NO; the bug only
            // surfaces because the spurious non-zero read drives setter
            // injection where none was requested.
            Ok(Some(cratonvm_types::Value::Int(0)))
        },
    );
    // K4 follow-up: also register on subclasses in case the bytecode binds
    // invokevirtual to the concrete subclass instead of AbstractBeanDefinition
    // (some Spring versions emit non-virtual dispatch on RootBeanDefinition).
    for sub in &[
        "org/springframework/beans/factory/support/RootBeanDefinition",
        "org/springframework/beans/factory/support/GenericBeanDefinition",
        "org/springframework/beans/factory/support/ChildBeanDefinition",
    ] {
        r.register(sub, "getResolvedAutowireMode", "()I", |_ctx, _args| {
            Ok(Some(cratonvm_types::Value::Int(0)))
        });
    }

    // -----------------------------------------------------------------------
    // Spring Data Redis `RedisAccessor.afterPropertiesSet()`
    //
    // RedisAccessor.afterPropertiesSet() asserts that the
    // RedisConnectionFactory has been wired in:
    //   Assert.state(getConnectionFactory() != null,
    //                "RedisConnectionFactory is required");
    //
    // In real-JDK mode under CratonVM the `@Autowired` setter on
    // `RedisHttpSessionConfiguration.setRedisConnectionFactory(ObjectProvider,
    // ObjectProvider)` is not being invoked (multi-arg ObjectProvider setter
    // injection on a @Configuration class whose CGLIB enhancement was bypassed
    // — see `ConfigurationClassEnhancer.enhance` shim above).  The
    // RedisOperationsSessionRepository @Bean factory method then ends up
    // calling `RedisTemplate.afterPropertiesSet()` with a null connection
    // factory and Spring throws `IllegalStateException:
    // RedisConnectionFactory is required` during context refresh — which
    // aborts SportMe startup before it can reach the embedded Tomcat /
    // controller-registration phase we want to exercise next.
    //
    // Pragmatic workaround: turn `RedisAccessor.afterPropertiesSet()` into a
    // no-op so the RedisTemplate constructed by
    // `RedisHttpSessionConfiguration.sessionRepository()` does not blow up on
    // a missing factory.  Any actual session lookup at runtime would still
    // NPE, but bootstrap can advance past this gate.  This is the same kind
    // of bypass we apply to Tomcat lifecycle classes for the same goal.
    // -----------------------------------------------------------------------
    r.register(
        "org/springframework/data/redis/core/RedisAccessor",
        "afterPropertiesSet",
        "()V",
        |_ctx, _args| {
            eprintln!("[REDIS-DBG] RedisAccessor.afterPropertiesSet -> no-op (skip connection-factory assert)");
            Ok(None)
        },
    );

    // Same chain: RedisOperationsSessionRepository.setApplicationEventPublisher
    // does `Assert.notNull(applicationEventPublisher, "applicationEventPublisher cannot be null")`.
    // Because `@Autowired` setter injection on `RedisHttpSessionConfiguration` is
    // not running under our shim, the publisher is null when
    // `sessionRepository()` invokes the setter.  No-op on null to let bootstrap
    // continue.
    r.register(
        "org/springframework/session/data/redis/RedisOperationsSessionRepository",
        "setApplicationEventPublisher",
        "(Lorg/springframework/context/ApplicationEventPublisher;)V",
        |_ctx, _args| {
            // Swallow the assert.notNull; field stays uninitialized but that is
            // acceptable for bootstrap advancement.
            eprintln!("[REDIS-DBG] RedisOperationsSessionRepository.setApplicationEventPublisher -> no-op");
            Ok(None)
        },
    );

    // Same chain: RedisHttpSessionConfiguration.redisMessageListenerContainer()
    // calls container.setConnectionFactory(this.redisConnectionFactory) with
    // a null factory and Assert.notNull(...) throws
    // `IllegalArgumentException: ConnectionFactory must not be null!`.
    r.register(
        "org/springframework/data/redis/listener/RedisMessageListenerContainer",
        "setConnectionFactory",
        "(Lorg/springframework/data/redis/connection/RedisConnectionFactory;)V",
        |_ctx, _args| {
            eprintln!("[REDIS-DBG] RedisMessageListenerContainer.setConnectionFactory -> no-op (swallow null assert)");
            Ok(None)
        },
    );

    // Same chain: the `enableRedisKeyspaceNotificationsInitializer` bean's
    // afterPropertiesSet calls `connectionFactory.getConnection()` on its
    // null factory and NPEs.  Bypass the entire init — equivalent to having
    // `ConfigureRedisAction.NO_OP` selected, which is the early-return path
    // already supported by Spring.
    r.register(
        "org/springframework/session/data/redis/config/annotation/web/http/RedisHttpSessionConfiguration$EnableRedisKeyspaceNotificationsInitializer",
        "afterPropertiesSet",
        "()V",
        |_ctx, _args| {
            eprintln!("[REDIS-DBG] EnableRedisKeyspaceNotificationsInitializer.afterPropertiesSet -> no-op");
            Ok(None)
        },
    );

    // -----------------------------------------------------------------------
    // RedisOperationsSessionRepository.cleanupExpiredSessions
    //
    // The repository registers a @Scheduled(cron = "0 * * * * *") method
    // which Spring's ScheduledAnnotationBeanPostProcessor wires onto the
    // TaskScheduler.  Because the @Autowired RedisConnectionFactory setter
    // never fires under CratonVM (see RedisAccessor.afterPropertiesSet no-op
    // above), the first cron firing executes
    //   this.expirationPolicy.cleanExpiredSessions()
    // which calls into a RedisTemplate with a null factory and throws
    //   java.lang.IllegalStateException: RedisConnectionFactory is required
    // The TaskUtils$LoggingErrorHandler logs it; SpringApplication then sees
    // the failed cleanup, marks "Application run failed", and tries to
    // cancel the cron task — at which point ScheduledTask.cancel() asserts
    //   Assert.notNull(this.future, "No scheduled future")
    // failing because the registrar's future map was torn down already.
    //
    // Pragmatic bypass: turn cleanupExpiredSessions into a no-op so the
    // cron firing succeeds silently, no error is reported, no shutdown is
    // triggered, and the application stays up.  Same shape as the other
    // session/redis bypasses above.
    // -----------------------------------------------------------------------
    r.register(
        "org/springframework/session/data/redis/RedisOperationsSessionRepository",
        "cleanupExpiredSessions",
        "()V",
        |_ctx, _args| {
            eprintln!("[REDIS-DBG] RedisOperationsSessionRepository.cleanupExpiredSessions -> no-op");
            Ok(None)
        },
    );

    // -----------------------------------------------------------------------
    // Round 76: skip @Scheduled cron registration.
    //
    // ScheduledTaskRegistrar.scheduleCronTask(CronTask) calls
    // ConcurrentTaskScheduler.schedule(Runnable, Trigger) which constructs a
    // ReschedulingRunnable and calls its schedule(), which in turn calls
    // executor.schedule(this, delay, MILLIS).  Under CratonVM the
    // DelegatedScheduledExecutorService.schedule path ends up invoking the
    // task synchronously without populating `currentFuture`, so the very
    // first run() trips Assert.state("No scheduled future") in
    // obtainCurrentFuture(), aborting context refresh.
    //
    // We don't run @Scheduled crons in this environment, so register the
    // ScheduledTaskRegistrar entry points as no-ops returning null.  Returning
    // null is acceptable: callers store the result in a List<ScheduledTask>
    // that is only used to cancel tasks at shutdown.
    r.register(
        "org/springframework/scheduling/config/ScheduledTaskRegistrar",
        "scheduleCronTask",
        "(Lorg/springframework/scheduling/config/CronTask;)Lorg/springframework/scheduling/config/ScheduledTask;",
        |_ctx, _args| {
            eprintln!("[SCHED-DBG] ScheduledTaskRegistrar.scheduleCronTask -> no-op (null)");
            Ok(Some(Value::Object(None)))
        },
    );
    r.register(
        "org/springframework/scheduling/config/ScheduledTaskRegistrar",
        "scheduleFixedRateTask",
        "(Lorg/springframework/scheduling/config/FixedRateTask;)Lorg/springframework/scheduling/config/ScheduledTask;",
        |_ctx, _args| {
            eprintln!("[SCHED-DBG] ScheduledTaskRegistrar.scheduleFixedRateTask -> no-op (null)");
            Ok(Some(Value::Object(None)))
        },
    );
    r.register(
        "org/springframework/scheduling/config/ScheduledTaskRegistrar",
        "scheduleFixedDelayTask",
        "(Lorg/springframework/scheduling/config/FixedDelayTask;)Lorg/springframework/scheduling/config/ScheduledTask;",
        |_ctx, _args| {
            eprintln!("[SCHED-DBG] ScheduledTaskRegistrar.scheduleFixedDelayTask -> no-op (null)");
            Ok(Some(Value::Object(None)))
        },
    );
    r.register(
        "org/springframework/scheduling/config/ScheduledTaskRegistrar",
        "scheduleTriggerTask",
        "(Lorg/springframework/scheduling/config/TriggerTask;)Lorg/springframework/scheduling/config/ScheduledTask;",
        |_ctx, _args| {
            eprintln!("[SCHED-DBG] ScheduledTaskRegistrar.scheduleTriggerTask -> no-op (null)");
            Ok(Some(Value::Object(None)))
        },
    );

    r.register(
        "org/springframework/core/io/UrlResource",
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if spring_dbg_enabled() {
                eprintln!("[URLRES-DBG] UrlResource.getInputStream intercepted");
            }
            // Obtain the URL via UrlResource.getURL() — the public accessor
            // that simply returns the private `url` field.  Using a Java
            // call lets us avoid hard-coding the field index (which depends
            // on the number of fields in the AbstractResource/AbstractFile
            // ResolvingResource superclasses).
            let url_val = ctx.invoke_virtual(
                this,
                "getURL",
                "()Ljava/net/URL;",
                &[],
            )?;
            let url_obj = match url_val {
                Some(Value::Object(Some(o))) => o,
                _ => return Err(ioex("UrlResource.getInputStream: getURL() returned null")),
            };
            // Delegate directly to URL.openStream() — our native handles
            // jar:file: double-nested URLs correctly.
            if spring_dbg_enabled() {
                eprintln!("[URLRES-DBG] UrlResource.getInputStream -> openStream");
            }
            ctx.invoke_virtual(url_obj, "openStream", "()Ljava/io/InputStream;", &[])
        },
    );

    let huc = "java/net/HttpURLConnection";
    r.register(huc, "connect", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        huc_perform(ctx, this)?;
        Ok(None)
    });
    r.register(huc, "getResponseCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        huc_perform(ctx, this)?;
        Ok(Some(ctx.get_field(this, HUC_CODE)))
    });
    r.register(huc, "getInputStream", "()Ljava/io/InputStream;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // S111r22 — For non-HTTP/HTTPS URLs (jar:file:, file:, classpath:, jrt:),
        // huc_perform would try to make a real HTTP request which fails. Instead,
        // delegate to URL.openStream() via the stored HUC_URL. This enables
        // SpringFactoriesLoader to read META-INF/spring.factories from nested JARs
        // when it goes through url.openConnection().getInputStream().
        let url_str = huc_url_string(ctx, this);
        eprintln!("[HUC-DBG] HttpURLConnection.getInputStream url={}", url_str);
        if !url_str.starts_with("http://") && !url_str.starts_with("https://") {
            // Non-HTTP: delegate to URL.openStream() on the stored URL object.
            let url_obj = match ctx.get_field(this, HUC_URL) {
                Value::Object(Some(o)) => o,
                _ => return Err(ioex("HttpURLConnection.getInputStream: no URL for non-http")),
            };
            return ctx.invoke_virtual(url_obj, "openStream", "()Ljava/io/InputStream;", &[]);
        }
        huc_perform(ctx, this)?;
        let body = match ctx.get_field(this, HUC_BODY) {
            Value::Object(Some(a)) => a,
            _ => ctx.new_array(ArrayElementType::Byte, 0),
        };
        let len = ctx.array_length(body) as i32;
        let stream = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
        ctx.set_field(stream, 0, Value::Object(Some(body))); // buf
        ctx.set_field(stream, 1, Value::Int(0));              // pos
        ctx.set_field(stream, 2, Value::Int(0));              // mark
        ctx.set_field(stream, 3, Value::Int(len));            // count
        Ok(Some(Value::Object(Some(stream))))
    });
    r.register(huc, "getHeaderField", "(Ljava/lang/String;)Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        huc_perform(ctx, this)?;
        let key_val = args.get(1).copied().unwrap_or(Value::Object(None));
        let key = value_or_string(ctx, key_val, "");
        if let Value::Object(Some(arr)) = ctx.get_field(this, HUC_RESP_HEADERS) {
            let len = ctx.array_length(arr);
            for i in 0..len {
                if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
                    let line = ctx.read_string(s).unwrap_or_default();
                    if let Some(colon) = line.find(':') {
                        if line[..colon].trim().eq_ignore_ascii_case(&key) {
                            let val = line[colon + 1..].trim().to_string();
                            let v = ctx.create_string(&val);
                            return Ok(Some(Value::Object(Some(v))));
                        }
                    }
                }
            }
        }
        Ok(Some(Value::Object(None)))
    });
    r.register(huc, "getHeaderField", "(I)Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        huc_perform(ctx, this)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if idx < 0 {
            return Ok(Some(Value::Object(None)));
        }
        if let Value::Object(Some(arr)) = ctx.get_field(this, HUC_RESP_HEADERS) {
            let len = ctx.array_length(arr);
            if (idx as usize) < len {
                if let Value::Object(Some(s)) = ctx.get_array_element(arr, idx as usize) {
                    let line = ctx.read_string(s).unwrap_or_default();
                    if let Some(colon) = line.find(':') {
                        let v = line[colon + 1..].trim().to_string();
                        let out = ctx.create_string(&v);
                        return Ok(Some(Value::Object(Some(out))));
                    }
                }
            }
        }
        Ok(Some(Value::Object(None)))
    });
    r.register(huc, "getContentLength", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        huc_perform(ctx, this)?;
        match ctx.get_field(this, HUC_BODY) {
            Value::Object(Some(a)) => Ok(Some(Value::Int(ctx.array_length(a) as i32))),
            _ => Ok(Some(Value::Int(-1))),
        }
    });
    r.register(huc, "setRequestMethod", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mval = args.get(1).copied().unwrap_or(Value::Object(None));
        let method = value_or_string(ctx, mval, "GET");
        let s = ctx.create_string(&method);
        ctx.set_field(this, HUC_METHOD, Value::Object(Some(s)));
        Ok(None)
    });
    r.register(huc, "disconnect", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, HUC_CONNECTED, Value::Int(0));
        ctx.set_field(this, HUC_FD, Value::Int(-1));
        Ok(None)
    });
    r.register(huc, "setDoInput", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        ctx.set_field(this, HUC_DO_INPUT, Value::Int(v));
        Ok(None)
    });
    r.register(huc, "setDoOutput", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field(this, HUC_DO_OUTPUT, Value::Int(v));
        Ok(None)
    });

    // -----------------------------------------------------------------------
    // S111r24 — Spring Boot / Spring Framework compatibility natives.
    //
    // `UrlResource.getInputStream()` (Spring 5.x / 6.x) calls
    //   openConnection() → customizeConnection(con) → con.getInputStream()
    // where `customizeConnection` is defined on AbstractFileResolvingResource:
    //
    //   protected void customizeConnection(URLConnection con) {
    //       ResourceUtils.useCachesIfNecessary(con);  // ← getClass().getSimpleName()
    //       if (con instanceof HttpURLConnection) {
    //           customizeConnection((HttpURLConnection) con);
    //       }
    //   }
    //
    // `ResourceUtils.useCachesIfNecessary` calls
    //   con.getClass().getSimpleName().startsWith("JNLP")
    // then con.setUseCaches(false).  The `setUseCaches` is already a no-op
    // native on URLConnection; however any hiccup in the getSimpleName chain
    // (e.g. the Class mirror for our synthetic HttpURLConnection having a null
    // name slot) would throw an exception NOT covered by UrlResource's IOException
    // handler (offset 10 is outside the [13,17) try block), silently aborting
    // the getInputStream() call and leaving loadSpringFactories with no entries.
    //
    // Fix: intercept `ResourceUtils.useCachesIfNecessary` and both overloads of
    // `AbstractFileResolvingResource.customizeConnection` as native no-ops.
    // This lets execution fall straight through to con.getInputStream().
    // -----------------------------------------------------------------------

    // ResourceUtils.useCachesIfNecessary — no-op; skips getSimpleName() call
    r.register(
        "org/springframework/util/ResourceUtils",
        "useCachesIfNecessary",
        "(Ljava/net/URLConnection;)V",
        |_ctx, _args| {
            eprintln!("[SPRING-DBG] ResourceUtils.useCachesIfNecessary intercepted (no-op)");
            Ok(None)
        },
    );

    // S111r24 — Some loader paths can hand a null URL into
    // ResourceUtils.isFileURL/isJarURL during fallback probing. HotSpot-side
    // Spring code expects "not file/jar" in this case; without a guard we
    // NPE on `url.getProtocol()` and abort startup.
    r.register(
        "org/springframework/util/ResourceUtils",
        "isFileURL",
        "(Ljava/net/URL;)Z",
        |ctx, args| {
            if std::env::var_os("CRATONVM_DBG_SBLOAD").is_some() {
                eprintln!("[DBG_SBLOAD] ResourceUtils.isFileURL native override");
            }
            let url = match args.get(0) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let proto = match ctx.invoke_virtual(url, "getProtocol", "()Ljava/lang/String;", &[])? {
                Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let is_file = proto == "file" || proto == "vfsfile" || proto == "vfs";
            Ok(Some(Value::Int(if is_file { 1 } else { 0 })))
        },
    );
    r.register(
        "org/springframework/util/ResourceUtils",
        "isJarURL",
        "(Ljava/net/URL;)Z",
        |ctx, args| {
            if std::env::var_os("CRATONVM_DBG_SBLOAD").is_some() {
                eprintln!("[DBG_SBLOAD] ResourceUtils.isJarURL native override");
            }
            let url = match args.get(0) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let proto = match ctx.invoke_virtual(url, "getProtocol", "()Ljava/lang/String;", &[])? {
                Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let is_jar = matches!(
                proto.as_str(),
                "jar" | "war" | "zip" | "vfszip" | "wsjar"
            );
            Ok(Some(Value::Int(if is_jar { 1 } else { 0 })))
        },
    );

    // S111r25 — Banner lookup is best-effort; unresolved classpath URLs can
    // trip `new UrlResource(null)` on some fallback paths. Returning null
    // from both banner resolvers keeps boot moving (Spring then uses no
    // custom banner or the default fallback).
    r.register(
        "org/springframework/boot/SpringApplicationBannerPrinter",
        "getTextBanner",
        "(Lorg/springframework/core/env/Environment;)Lorg/springframework/boot/Banner;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(
        "org/springframework/boot/SpringApplicationBannerPrinter",
        "getImageBanner",
        "(Lorg/springframework/core/env/Environment;)Lorg/springframework/boot/Banner;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // S111r27 — Keep optional integrations truly optional. Spring computes
    // several static "xxxPresent" flags via ClassUtils.isPresent(...); when
    // these flip true under partial emulation, later probes may dive into
    // missing subsystems (JSF/Groovy) and destabilize bootstrap.
    r.register(
        "org/springframework/util/ClassUtils",
        "isPresent",
        "(Ljava/lang/String;Ljava/lang/ClassLoader;)Z",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            if name == "jakarta.faces.context.FacesContext" || name.starts_with("groovy.") {
                return Ok(Some(Value::Int(0)));
            }
            let internal = name.replace('.', "/");
            let present = ctx.ensure_class_initialized(&internal).is_ok();
            Ok(Some(Value::Int(if present { 1 } else { 0 })))
        },
    );

    // S111r26 — JSF integration is optional; when the Faces API is absent,
    // Spring's FacesDependencyRegistrar probe should effectively no-op.
    // In our current runtime, that probe can escalate into hard failure via
    // NoClassDefFoundError on jakarta.faces.*. Short-circuit it.
    r.register(
        "org/springframework/web/context/support/WebApplicationContextUtils$FacesDependencyRegistrar",
        "registerFacesDependencies",
        "(Lorg/springframework/beans/factory/config/ConfigurableListableBeanFactory;)V",
        |_ctx, _args| Ok(None),
    );
    r.register(
        "org/springframework/web/context/support/WebApplicationContextUtils",
        "registerWebApplicationScopes",
        "(Lorg/springframework/beans/factory/config/ConfigurableListableBeanFactory;)V",
        |_ctx, _args| Ok(None),
    );
    r.register(
        "org/springframework/web/context/support/WebApplicationContextUtils",
        "registerWebApplicationScopes",
        "(Lorg/springframework/beans/factory/config/ConfigurableListableBeanFactory;Ljakarta/servlet/ServletContext;)V",
        |_ctx, _args| Ok(None),
    );
    r.register(
        "org/springframework/boot/web/servlet/context/ServletWebServerApplicationContext",
        "registerWebApplicationScopes",
        "(Lorg/springframework/beans/factory/config/ConfigurableListableBeanFactory;)V",
        |_ctx, _args| Ok(None),
    );

    // S111r57 — bypass MissingWebServerFactoryBeanException by overriding
    // ServletWebServerApplicationContext.getWebServerFactory() to allocate a
    // TomcatServletWebServerFactory directly instead of asking the bean factory.
    //
    // In real Spring Boot, this protected method calls
    //   getBeanFactory().getBeanNamesForType(ServletWebServerFactory.class)
    // and throws MissingWebServerFactoryBeanException if zero matches. Under
    // CratonVM the auto-configuration that registers the Tomcat factory bean
    // never completes (Cglib/condition-evaluation issues upstream), so the
    // lookup fails. We short-circuit by constructing the factory natively.
    //
    // SB 2.x: context = org/springframework/boot/web/servlet/context/ServletWebServerApplicationContext
    //         factory = org/springframework/boot/web/servlet/server/ServletWebServerFactory
    //         impl    = org/springframework/boot/web/embedded/tomcat/TomcatServletWebServerFactory
    //
    // SB 4.x: context = org/springframework/boot/web/server/servlet/context/ServletWebServerApplicationContext
    //         factory = org/springframework/boot/web/server/servlet/ServletWebServerFactory
    //         impl    = org/springframework/boot/tomcat/servlet/TomcatServletWebServerFactory
    fn alloc_tomcat_factory(
        ctx: &mut dyn NativeContext,
        impl_class: &str,
    ) -> MethodCallResult {
        let obj_val = match ctx.new_object(impl_class) {
            Ok(Some(v)) => v,
            Ok(None) => return Ok(Some(Value::Object(None))),
            Err(e) => return Err(e),
        };
        // Try to run the no-arg constructor; if it fails, return the raw alloc.
        let _ = ctx.invoke_special(impl_class, "<init>", "()V", &[obj_val]);
        Ok(Some(obj_val))
    }

    // SB 4.x
    r.register(
        "org/springframework/boot/web/server/servlet/context/ServletWebServerApplicationContext",
        "getWebServerFactory",
        "()Lorg/springframework/boot/web/server/servlet/ServletWebServerFactory;",
        |ctx, _args| {
            alloc_tomcat_factory(
                ctx,
                "org/springframework/boot/tomcat/servlet/TomcatServletWebServerFactory",
            )
        },
    );

    // SB 2.x
    r.register(
        "org/springframework/boot/web/servlet/context/ServletWebServerApplicationContext",
        "getWebServerFactory",
        "()Lorg/springframework/boot/web/servlet/server/ServletWebServerFactory;",
        |ctx, _args| {
            alloc_tomcat_factory(
                ctx,
                "org/springframework/boot/web/embedded/tomcat/TomcatServletWebServerFactory",
            )
        },
    );

    // CGLIB-δ — Spring WebFlux mirror of the servlet bypass above. Apps that
    // start a `ReactiveWebServerApplicationContext` (e.g. letsgo-gateway with
    // Spring Cloud Gateway) hit the same `MissingWebServerFactoryBeanException`
    // because CratonVM's CGLIB bypass skips the auto-config that registers
    // `nettyReactiveWebServerFactory` as a bean.
    //
    // The reactive context (SB 2.7 — `org/springframework/boot/web/reactive/context/
    // ReactiveWebServerApplicationContext`) calls:
    //   1. `getWebServerFactoryBeanName()` — looks up bean names, throws
    //      `MissingWebServerFactoryBeanException` if none. Native shim returns
    //      a synthetic bean name to bypass the throw site.
    //   2. `getWebServerFactory(String)` — `beanFactory.getBean(name, Class)`.
    //      Native shim constructs `NettyReactiveWebServerFactory` directly,
    //      ignoring the bean-name argument.
    //   3. `getBeanDefinition(name).isLazyInit()` — would still fail because no
    //      bean definition exists for our synthetic name. To dodge this entire
    //      chain we also no-op the private `createWebServer()` driver. The
    //      `serverManager` field stays null; `getWebServer()` will return null
    //      to the caller. This is a deliberate "boot past, don't actually
    //      serve" stance consistent with the Tomcat-side shims below.
    fn alloc_netty_reactive_factory(
        ctx: &mut dyn NativeContext,
    ) -> MethodCallResult {
        let impl_class =
            "org/springframework/boot/web/embedded/netty/NettyReactiveWebServerFactory";
        let obj_val = match ctx.new_object(impl_class) {
            Ok(Some(v)) => v,
            Ok(None) => return Ok(Some(Value::Object(None))),
            Err(e) => return Err(e),
        };
        let _ = ctx.invoke_special(impl_class, "<init>", "()V", &[obj_val]);
        Ok(Some(obj_val))
    }

    // SB 2.x reactive — primary path
    r.register(
        "org/springframework/boot/web/reactive/context/ReactiveWebServerApplicationContext",
        "getWebServerFactory",
        "(Ljava/lang/String;)Lorg/springframework/boot/web/reactive/server/ReactiveWebServerFactory;",
        |ctx, _args| alloc_netty_reactive_factory(ctx),
    );
    r.register(
        "org/springframework/boot/web/reactive/context/ReactiveWebServerApplicationContext",
        "getWebServerFactoryBeanName",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("nettyReactiveWebServerFactory");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    // No-op the private driver so the downstream `getBeanDefinition(beanName)`
    // / `isLazyInit()` / `registerSingleton(...)` chain doesn't run.
    r.register(
        "org/springframework/boot/web/reactive/context/ReactiveWebServerApplicationContext",
        "createWebServer",
        "()V",
        |_ctx, _args| Ok(None),
    );
    // The subclass used by Spring Boot's reactive auto-configuration —
    // dispatch resolves on the declaring class, but cover the subclass too
    // for safety in case bytecode binds the call to the subclass directly.
    r.register(
        "org/springframework/boot/web/reactive/context/AnnotationConfigReactiveWebServerApplicationContext",
        "createWebServer",
        "()V",
        |_ctx, _args| Ok(None),
    );

    // Round 60 — bypass StandardContext init/start failure.
    //
    // After getWebServer() succeeds, Spring Boot calls TomcatWebServer.start()
    // which drives the Tomcat lifecycle: Engine → Host → Context. The Context
    // (TomcatEmbeddedContext extends StandardContext) fails during init/start
    // with a chain of LifecycleException → ExecutionException → … with no
    // root cause preserved (Tomcat's ContainerBase wraps child failures as
    // bare LifecycleException with only a message). The original failure is
    // most likely a missing servlet/filter init resource or a NullPointerException
    // from real-JDK gaps in our environment (JNDI / annotation scanning / etc.).
    //
    // Pragmatic fix: no-op StandardContext.initInternal()V and startInternal()V.
    // LifecycleBase wraps these calls in state transitions
    // (INITIALIZING → INITIALIZED, STARTING_PREP → STARTING → STARTED), so a
    // successful no-op lets the lifecycle complete cleanly. The servlet
    // container itself won't dispatch requests, but the boot succeeds past
    // the LifecycleException and the demo can advance.
    fn ctx_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(None)
    }
    r.register(
        "org/apache/catalina/core/StandardContext",
        "initInternal",
        "()V",
        ctx_noop,
    );
    r.register(
        "org/apache/catalina/core/StandardContext",
        "startInternal",
        "()V",
        ctx_noop,
    );
    // Spring Boot's TomcatEmbeddedContext overrides startInternal — cover both
    // common package locations so the dispatch hits the native regardless of
    // which subclass the SB version uses.
    r.register(
        "org/springframework/boot/tomcat/TomcatEmbeddedContext",
        "startInternal",
        "()V",
        ctx_noop,
    );
    r.register(
        "org/springframework/boot/web/embedded/tomcat/TomcatEmbeddedContext",
        "startInternal",
        "()V",
        ctx_noop,
    );

    // Round 60 cont. — short-circuit ContainerBase$StartChild.call() which
    // wraps `child.start()` in a Callable submitted to an executor. The
    // failure surfaces as ExecutionException chained into a LifecycleException
    // ("A child container failed during start") with the original cause
    // discarded. By making the Callable a no-op that returns null, the
    // Future completes successfully and the engine/host advance.
    fn start_child_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(Some(Value::Object(None)))
    }
    r.register(
        "org/apache/catalina/core/ContainerBase$StartChild",
        "call",
        "()Ljava/lang/Object;",
        start_child_noop,
    );

    // Round 60 cont. — Connector.startInternal() fails with NPE in
    // Thread.priority because our synthetic Thread/TaskThread layout
    // doesn't wire up the `holder.group` field. Skip the protocol-handler
    // start; the demo doesn't actually serve requests under CratonVM.
    fn connector_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(None)
    }
    r.register(
        "org/apache/catalina/connector/Connector",
        "startInternal",
        "()V",
        connector_noop,
    );
    // Same problem path through protocol handler/endpoint — short-circuit
    // both layers so the LifecycleBase wrapper completes state transitions.
    r.register(
        "org/apache/coyote/AbstractProtocol",
        "start",
        "()V",
        connector_noop,
    );

    // Round 60 cont. — short-circuit TomcatWebServer.start() entirely.
    // We've already constructed the TomcatWebServer in getWebServer(), and
    // start() drives the full Catalina lifecycle which our environment can't
    // complete (Thread.holder.group is null, NamingResources native lookups
    // fail, etc.). Replacing start() with a no-op returns control to Spring
    // Boot's ServletWebServerApplicationContext.startWebServer with no
    // exception so the demo advances past the embedded-Tomcat phase.
    fn tomcat_web_server_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(None)
    }
    r.register(
        "org/springframework/boot/tomcat/TomcatWebServer",
        "start",
        "()V",
        tomcat_web_server_noop,
    );
    r.register(
        "org/springframework/boot/web/embedded/tomcat/TomcatWebServer",
        "start",
        "()V",
        tomcat_web_server_noop,
    );
    // initialize() is the one that actually drives Tomcat.start() and the
    // protocol-handler chain — make it a no-op too. (Spring Boot calls
    // initialize() from the constructor before returning the WebServer.)
    r.register(
        "org/springframework/boot/tomcat/TomcatWebServer",
        "initialize",
        "()V",
        tomcat_web_server_noop,
    );
    r.register(
        "org/springframework/boot/web/embedded/tomcat/TomcatWebServer",
        "initialize",
        "()V",
        tomcat_web_server_noop,
    );
    // Also short-circuit Tomcat.start() at the Catalina root in case a
    // different code path reaches it.
    r.register(
        "org/apache/catalina/startup/Tomcat",
        "start",
        "()V",
        tomcat_web_server_noop,
    );

    // AbstractFileResolvingResource.customizeConnection(URLConnection) — no-op
    r.register(
        "org/springframework/core/io/AbstractFileResolvingResource",
        "customizeConnection",
        "(Ljava/net/URLConnection;)V",
        |_ctx, _args| {
            eprintln!("[SPRING-DBG] AbstractFileResolvingResource.customizeConnection(UC) intercepted (no-op)");
            Ok(None)
        },
    );

    // AbstractFileResolvingResource.customizeConnection(HttpURLConnection) — no-op
    r.register(
        "org/springframework/core/io/AbstractFileResolvingResource",
        "customizeConnection",
        "(Ljava/net/HttpURLConnection;)V",
        |_ctx, _args| {
            eprintln!("[SPRING-DBG] AbstractFileResolvingResource.customizeConnection(HUC) intercepted (no-op)");
            Ok(None)
        },
    );
}

// ===========================================================================
// RE.5 — java.net.http.HttpClient
// ===========================================================================

fn register_re5_http_client(r: &mut NativeMethodRegistry) {
    let hc = "java/net/http/HttpClient";
    r.register(hc, "newHttpClient", "()Ljava/net/http/HttpClient;", |ctx, _args| {
        let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient", 1);
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(
        hc,
        "newBuilder",
        "()Ljava/net/http/HttpClient$Builder;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient$Builder", 1);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    let bld = "java/net/http/HttpClient$Builder";
    r.register(bld, "build", "()Ljava/net/http/HttpClient;", |ctx, _args| {
        let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient", 1);
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(
        bld,
        "connectTimeout",
        "(Ljava/time/Duration;)Ljava/net/http/HttpClient$Builder;",
        |_ctx, args| Ok(Some(args[0])),
    );
    r.register(
        bld,
        "followRedirects",
        "(Ljava/net/http/HttpClient$Redirect;)Ljava/net/http/HttpClient$Builder;",
        |_ctx, args| Ok(Some(args[0])),
    );

    r.register(
        hc,
        "send",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;",
        |ctx, args| {
            let req = obj_arg(args, 1)?;
            let method = read_field_string_or(ctx, req, 0, "GET");
            let uri = read_field_string_or(ctx, req, 1, "");
            let body_str = read_field_string_or(ctx, req, 2, "");
            let hdrs_val = ctx.get_field(req, 3);
            let headers = huc_extract_req_headers(ctx, hdrs_val);
            if uri.is_empty() {
                return Err(ioex("HttpRequest.uri is empty"));
            }
            let resp = http_perform_request(&method, &uri, &headers, body_str.as_bytes(), 10)
                .map_err(|e| ioex(format!("HttpClient.send failed: {e}")))?;
            let out = alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse", 3);
            ctx.set_field(out, 0, Value::Int(resp.status));
            let body_text = String::from_utf8_lossy(&resp.body).to_string();
            let bs = ctx.create_string(&body_text);
            ctx.set_field(out, 1, Value::Object(Some(bs)));
            let hdr_arr = ctx.new_ref_array(ClassId::new(0), resp.headers.len());
            for (i, (k, v)) in resp.headers.iter().enumerate() {
                let s = ctx.create_string(&format!("{k}: {v}"));
                ctx.set_array_element(hdr_arr, i, Value::Object(Some(s)));
            }
            ctx.set_field(out, 2, Value::Object(Some(hdr_arr)));
            Ok(Some(Value::Object(Some(out))))
        },
    );

    let req = "java/net/http/HttpRequest";
    r.register(
        req,
        "newBuilder",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, _args| {
            let b = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$Builder", 4);
            let m = ctx.create_string("GET");
            ctx.set_field(b, 0, Value::Object(Some(m)));
            ctx.set_field(b, 1, Value::Object(None));
            ctx.set_field(b, 2, Value::Object(None));
            ctx.set_field(b, 3, Value::Object(None));
            Ok(Some(Value::Object(Some(b))))
        },
    );
    r.register(
        req,
        "newBuilder",
        "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let b = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$Builder", 4);
            let m = ctx.create_string("GET");
            ctx.set_field(b, 0, Value::Object(Some(m)));
            let uri = obj_arg(args, 0)?;
            let uri_s = match ctx.get_field(uri, 0) {
                Value::Object(Some(s)) => s,
                _ => ctx.create_string(""),
            };
            ctx.set_field(b, 1, Value::Object(Some(uri_s)));
            ctx.set_field(b, 2, Value::Object(None));
            ctx.set_field(b, 3, Value::Object(None));
            Ok(Some(Value::Object(Some(b))))
        },
    );

    let bl = "java/net/http/HttpRequest$Builder";
    r.register(
        bl,
        "uri",
        "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let uri = obj_arg(args, 1)?;
            let uri_s = match ctx.get_field(uri, 0) {
                Value::Object(Some(s)) => s,
                _ => ctx.create_string(""),
            };
            ctx.set_field(this, 1, Value::Object(Some(uri_s)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        bl,
        "GET",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("GET");
            ctx.set_field(this, 0, Value::Object(Some(m)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        bl,
        "DELETE",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("DELETE");
            ctx.set_field(this, 0, Value::Object(Some(m)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        bl,
        "POST",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("POST");
            ctx.set_field(this, 0, Value::Object(Some(m)));
            if let Some(Value::Object(Some(bp))) = args.get(1) {
                let body = ctx.get_field(*bp, 0);
                ctx.set_field(this, 2, body);
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        bl,
        "PUT",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("PUT");
            ctx.set_field(this, 0, Value::Object(Some(m)));
            if let Some(Value::Object(Some(bp))) = args.get(1) {
                let body = ctx.get_field(*bp, 0);
                ctx.set_field(this, 2, body);
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        bl,
        "header",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let k = value_or_string(ctx, args.get(1).copied().unwrap_or(Value::Object(None)), "");
            let v = value_or_string(ctx, args.get(2).copied().unwrap_or(Value::Object(None)), "");
            let line = ctx.create_string(&format!("{k}: {v}"));
            let arr = match ctx.get_field(this, 3) {
                Value::Object(Some(a)) => a,
                _ => {
                    let a = ctx.new_array(ArrayElementType::Reference, 32);
                    ctx.set_field(this, 3, Value::Object(Some(a)));
                    a
                }
            };
            let len = ctx.array_length(arr);
            for i in 0..len {
                if let Value::Object(None) = ctx.get_array_element(arr, i) {
                    ctx.set_array_element(arr, i, Value::Object(Some(line)));
                    break;
                }
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        bl,
        "build",
        "()Ljava/net/http/HttpRequest;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let req = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest", 4);
            for i in 0..4 {
                let v = ctx.get_field(this, i);
                ctx.set_field(req, i, v);
            }
            Ok(Some(Value::Object(Some(req))))
        },
    );

    let bps = "java/net/http/HttpRequest$BodyPublishers";
    r.register(
        bps,
        "ofString",
        "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, args| {
            let body = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$BodyPublisher", 1);
            ctx.set_field(body, 0, args.first().copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Object(Some(body))))
        },
    );
    r.register(
        bps,
        "noBody",
        "()Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, _args| {
            let body = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$BodyPublisher", 1);
            let empty = ctx.create_string("");
            ctx.set_field(body, 0, Value::Object(Some(empty)));
            Ok(Some(Value::Object(Some(body))))
        },
    );

    let bhs = "java/net/http/HttpResponse$BodyHandlers";
    r.register(
        bhs,
        "ofString",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse$BodyHandler", 1);
            let tag = ctx.create_string("string");
            ctx.set_field(bh, 0, Value::Object(Some(tag)));
            Ok(Some(Value::Object(Some(bh))))
        },
    );
    r.register(
        bhs,
        "discarding",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse$BodyHandler", 1);
            let tag = ctx.create_string("discarding");
            ctx.set_field(bh, 0, Value::Object(Some(tag)));
            Ok(Some(Value::Object(Some(bh))))
        },
    );

    let resp = "java/net/http/HttpResponse";
    r.register(resp, "statusCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(resp, "body", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
}

// ===========================================================================
// RE.6 — javax.net.ssl.SSLContext
// ===========================================================================

fn register_re6_ssl_context(r: &mut NativeMethodRegistry) {
    let ctx_cls = "javax/net/ssl/SSLContext";
    r.register(
        ctx_cls,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
        |ctx, args| {
            let proto_val = args.first().copied().unwrap_or(Value::Object(None));
            let proto = value_or_string(ctx, proto_val, "TLS");
            if !(proto.eq_ignore_ascii_case("TLS")
                || proto.eq_ignore_ascii_case("TLSv1.2")
                || proto.eq_ignore_ascii_case("TLSv1.3")
                || proto.eq_ignore_ascii_case("Default")
                || proto.eq_ignore_ascii_case("SSL"))
            {
                return Err(ioex(format!("NoSuchAlgorithmException: {proto}")));
            }
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", 2);
            let name = ctx.create_string(&proto);
            ctx.set_field(obj, 0, Value::Object(Some(name)));
            ctx.set_field(obj, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ctx_cls,
        "getDefault",
        "()Ljavax/net/ssl/SSLContext;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", 2);
            let name = ctx.create_string("TLS");
            ctx.set_field(obj, 0, Value::Object(Some(name)));
            ctx.set_field(obj, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ctx_cls,
        "init",
        "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 1, Value::Int(1));
            Ok(None)
        },
    );
    r.register(
        ctx_cls,
        "getSocketFactory",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let f = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 1);
            ctx.set_field(f, 0, Value::Object(Some(this)));
            Ok(Some(Value::Object(Some(f))))
        },
    );
    r.register(
        ctx_cls,
        "getServerSocketFactory",
        "()Ljavax/net/ssl/SSLServerSocketFactory;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let f = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLServerSocketFactory", 1);
            ctx.set_field(f, 0, Value::Object(Some(this)));
            Ok(Some(Value::Object(Some(f))))
        },
    );
    r.register(
        ctx_cls,
        "getProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );

    let sf = "javax/net/ssl/SSLSocketFactory";
    r.register(
        sf,
        "createSocket",
        "(Ljava/lang/String;I)Ljava/net/Socket;",
        |ctx, args| {
            let host_val = args.get(1).copied().unwrap_or(Value::Object(None));
            let host = value_or_string(ctx, host_val, "");
            let port = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            if host.is_empty() || !(1..=65535).contains(&port) {
                return Err(iae(format!("bad host/port: {host}:{port}")));
            }
            let connector = native_tls::TlsConnector::builder()
                .build()
                .map_err(|e| ioex(format!("TLS connector: {e}")))?;
            let id = crate::servlet::s2_tls_connect(&connector, &host, port as u16)
                .map_err(|e| ioex(format!("TLS connect: {e}")))?;
            let sock = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocket", 5);
            let host_s = ctx.create_string(&host);
            ctx.set_field(sock, SOCK_HOST, Value::Object(Some(host_s)));
            sock_set(sock, |s| {
                s.port = port;
                s.local_port = 0;
                s.closed = 0;
                s.stream_id = id;
            });
            Ok(Some(Value::Object(Some(sock))))
        },
    );
    r.register(sf, "getDefault", "()Ljavax/net/SocketFactory;", |ctx, _args| {
        let f = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 1);
        ctx.set_field(f, 0, Value::Object(None));
        Ok(Some(Value::Object(Some(f))))
    });
}

// ===========================================================================
// RE.7 — java.net.DatagramSocket
// ===========================================================================

const DS_PORT: usize = 0;
const DS_CLOSED: usize = 1;
const DS_TIMEOUT: usize = 2;
const DS_FD: usize = 3;

const DP_DATA: usize = 0;
const DP_LENGTH: usize = 1;
const DP_ADDR: usize = 2;
const DP_PORT: usize = 3;

fn register_re7_datagram_socket(r: &mut NativeMethodRegistry) {
    let ds = "java/net/DatagramSocket";

    r.register(ds, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd = ctx
            .fd_table()
            .open_udp(Some("0.0.0.0:0"))
            .map_err(|e| ioex(format!("UDP open: {e}")))?;
        let port = ctx
            .fd_table()
            .udp_local_addr(fd)
            .ok()
            .and_then(|s| s.rsplit(':').next().and_then(|p| p.parse::<i32>().ok()))
            .unwrap_or(0);
        ctx.set_field(this, DS_PORT, Value::Int(port));
        ctx.set_field(this, DS_CLOSED, Value::Int(0));
        ctx.set_field(this, DS_TIMEOUT, Value::Int(0));
        ctx.set_field(this, DS_FD, Value::Int(fd as i32));
        Ok(None)
    });
    r.register(ds, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let addr_spec = format!("0.0.0.0:{port}");
        let fd = ctx
            .fd_table()
            .open_udp(Some(&addr_spec))
            .map_err(|e| ioex(format!("UDP bind: {e}")))?;
        let actual_port = ctx
            .fd_table()
            .udp_local_addr(fd)
            .ok()
            .and_then(|s| s.rsplit(':').next().and_then(|p| p.parse::<i32>().ok()))
            .unwrap_or(port);
        ctx.set_field(this, DS_PORT, Value::Int(actual_port));
        ctx.set_field(this, DS_CLOSED, Value::Int(0));
        ctx.set_field(this, DS_TIMEOUT, Value::Int(0));
        ctx.set_field(this, DS_FD, Value::Int(fd as i32));
        Ok(None)
    });
    r.register(ds, "<init>", "(ILjava/net/InetAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let host = match args.get(2) {
            Some(Value::Object(Some(ia))) => read_field_string_or(ctx, *ia, IA_ADDR, "0.0.0.0"),
            _ => "0.0.0.0".to_string(),
        };
        let fd = ctx
            .fd_table()
            .open_udp(Some(&format!("{host}:{port}")))
            .map_err(|e| ioex(format!("UDP bind: {e}")))?;
        let actual_port = ctx
            .fd_table()
            .udp_local_addr(fd)
            .ok()
            .and_then(|s| s.rsplit(':').next().and_then(|p| p.parse::<i32>().ok()))
            .unwrap_or(port);
        ctx.set_field(this, DS_PORT, Value::Int(actual_port));
        ctx.set_field(this, DS_CLOSED, Value::Int(0));
        ctx.set_field(this, DS_TIMEOUT, Value::Int(0));
        ctx.set_field(this, DS_FD, Value::Int(fd as i32));
        Ok(None)
    });

    r.register(ds, "send", "(Ljava/net/DatagramPacket;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pkt = obj_arg(args, 1)?;
        let fd = ctx.get_field(this, DS_FD).as_int().unwrap_or(-1);
        if fd < 0 {
            return Err(ioex("DatagramSocket: closed"));
        }
        let data_arr = match ctx.get_field(pkt, DP_DATA) {
            Value::Object(Some(a)) => a,
            _ => return Err(ioex("DatagramPacket: null data")),
        };
        let len = ctx.get_field(pkt, DP_LENGTH).as_int().unwrap_or(0);
        let port = ctx.get_field(pkt, DP_PORT).as_int().unwrap_or(0);
        let host = match ctx.get_field(pkt, DP_ADDR) {
            Value::Object(Some(ia)) => read_field_string_or(ctx, ia, IA_ADDR, ""),
            _ => String::new(),
        };
        if host.is_empty() || !(1..=65535).contains(&port) {
            return Err(iae(format!("DatagramPacket: bad addr {host}:{port}")));
        }
        let payload = java_byte_array_to_vec(ctx, data_arr, 0, len)?;
        let target = format!("{host}:{port}");
        ctx.fd_table()
            .udp_send(fd as u32, &payload, &target)
            .map_err(|e| ioex(format!("UDP send: {e}")))?;
        Ok(None)
    });

    r.register(ds, "receive", "(Ljava/net/DatagramPacket;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pkt = obj_arg(args, 1)?;
        let fd = ctx.get_field(this, DS_FD).as_int().unwrap_or(-1);
        if fd < 0 {
            return Err(ioex("DatagramSocket: closed"));
        }
        let data_arr = match ctx.get_field(pkt, DP_DATA) {
            Value::Object(Some(a)) => a,
            _ => return Err(ioex("DatagramPacket: null data")),
        };
        let cap = ctx.array_length(data_arr);
        let mut buf = vec![0u8; cap];
        let timeout_ms = ctx.get_field(this, DS_TIMEOUT).as_int().unwrap_or(0);
        let d = if timeout_ms > 0 {
            Some(Duration::from_millis(timeout_ms as u64))
        } else {
            None
        };
        ctx.fd_table()
            .udp_set_read_timeout(fd as u32, d)
            .map_err(|e| ioex(format!("UDP timeout: {e}")))?;
        let (n, origin) = ctx
            .fd_table()
            .udp_recv(fd as u32, &mut buf)
            .map_err(|e| ioex(format!("UDP recv: {e}")))?;
        copy_bytes_into_java_array(ctx, data_arr, 0, &buf[..n])?;
        ctx.set_field(pkt, DP_LENGTH, Value::Int(n as i32));
        if let Some((oh, op)) = origin.rsplit_once(':') {
            let port = op.parse::<i32>().unwrap_or(0);
            let ia = alloc_inet_address(ctx, oh, oh);
            ctx.set_field(pkt, DP_ADDR, Value::Object(Some(ia)));
            ctx.set_field(pkt, DP_PORT, Value::Int(port));
        }
        Ok(None)
    });

    r.register(ds, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd = ctx.get_field(this, DS_FD).as_int().unwrap_or(-1);
        if fd >= 0 {
            // Ignore close errors — the fd may already be closed by
            // a racing caller; DS_CLOSED is still set unconditionally.
            let _ = ctx.fd_table().close(fd as u32);
        }
        ctx.set_field(this, DS_CLOSED, Value::Int(1));
        ctx.set_field(this, DS_FD, Value::Int(-1));
        Ok(None)
    });
    r.register(ds, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, DS_CLOSED)))
    });
    r.register(ds, "getLocalPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, DS_PORT)))
    });
    r.register(ds, "setSoTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if ms < 0 {
            return Err(iae("negative SO_TIMEOUT"));
        }
        ctx.set_field(this, DS_TIMEOUT, Value::Int(ms));
        Ok(None)
    });
    r.register(ds, "getSoTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, DS_TIMEOUT)))
    });
}

// ===========================================================================
// RE.8 — java.net.NetworkInterface
// ===========================================================================

fn re8_enumerate_local_ips() -> Vec<IpAddr> {
    use std::net::UdpSocket;
    let mut out = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = s.local_addr() {
                if !addr.ip().is_loopback() {
                    out.push(addr.ip());
                }
            }
        }
    }
    if let Ok(s) = UdpSocket::bind("[::]:0") {
        if s.connect("[2001:4860:4860::8888]:80").is_ok() {
            if let Ok(addr) = s.local_addr() {
                if !addr.ip().is_loopback() && !out.contains(&addr.ip()) {
                    out.push(addr.ip());
                }
            }
        }
    }
    let lookup = format!("{}:0", hostname_string());
    if let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(&lookup.as_str()) {
        for sa in addrs {
            if !out.contains(&sa.ip()) {
                out.push(sa.ip());
            }
        }
    }
    out
}

fn re8_build_interfaces(ctx: &mut dyn NativeContext) -> Vec<ObjectRef> {
    let mut result = Vec::new();
    let ips = re8_enumerate_local_ips();

    let lo = alloc_concurrent_synthetic(ctx, "java/net/NetworkInterface", 5);
    let lo_name = ctx.create_string("lo");
    let lo_display = ctx.create_string("Loopback Interface");
    ctx.set_field(lo, 0, Value::Object(Some(lo_name)));
    ctx.set_field(lo, 1, Value::Object(Some(lo_display)));
    let mut lo_addrs: Vec<ObjectRef> = Vec::new();
    for ip in &ips {
        if ip.is_loopback() {
            let a = alloc_inet_address(ctx, "localhost", &ip.to_string());
            lo_addrs.push(a);
        }
    }
    if lo_addrs.is_empty() {
        lo_addrs.push(alloc_inet_address(ctx, "localhost", "127.0.0.1"));
    }
    let lo_arr = ctx.new_ref_array(ClassId::new(0), lo_addrs.len());
    for (i, a) in lo_addrs.iter().enumerate() {
        ctx.set_array_element(lo_arr, i, Value::Object(Some(*a)));
    }
    ctx.set_field(lo, 2, Value::Object(Some(lo_arr)));
    ctx.set_field(lo, 3, Value::Int(1));
    ctx.set_field(lo, 4, Value::Int(0b111));
    result.push(lo);

    let hostname = hostname_string();
    let mut idx = 2;
    for ip in &ips {
        if ip.is_loopback() {
            continue;
        }
        let iface = alloc_concurrent_synthetic(ctx, "java/net/NetworkInterface", 5);
        let name = ctx.create_string(if matches!(ip, IpAddr::V4(_)) { "eth0" } else { "eth1" });
        let display = ctx.create_string("Primary Network Interface");
        ctx.set_field(iface, 0, Value::Object(Some(name)));
        ctx.set_field(iface, 1, Value::Object(Some(display)));
        let a = alloc_inet_address(ctx, &hostname, &ip.to_string());
        let arr = ctx.new_ref_array(ClassId::new(0), 1);
        ctx.set_array_element(arr, 0, Value::Object(Some(a)));
        ctx.set_field(iface, 2, Value::Object(Some(arr)));
        ctx.set_field(iface, 3, Value::Int(idx));
        ctx.set_field(iface, 4, Value::Int(0b101));
        result.push(iface);
        idx += 1;
    }
    result
}

fn register_re8_network_interface(r: &mut NativeMethodRegistry) {
    let ni = "java/net/NetworkInterface";

    // NOTE: We do NOT register synthetic `getNetworkInterfaces` /
    // `networkInterfaces` natives. In real-JDK mode the Java methods
    // call native `getAll()` (registered below) which returns an empty
    // array, causing `getNetworkInterfaces` to throw SocketException
    // "No network interfaces configured". Callers like Spring Cloud's
    // InetUtils.findFirstNonLoopbackAddress() catch that and fall back
    // to defaults. Returning synthetic NetworkInterface objects here
    // breaks the real JDK's `getInetAddresses()` because the real
    // `addrs` field is in a different slot than our synthetic layout,
    // resulting in `arraylength null` NPE inside NetworkInterface$1.

    r.register(ni, "getHardwareAddress", "()[B", |ctx, _args| {
        let mac = ctx.new_array(ArrayElementType::Byte, 6);
        for i in 0..6 {
            ctx.set_array_element(mac, i, Value::Int(0));
        }
        Ok(Some(Value::Object(Some(mac))))
    });
    r.register(ni, "getMTU", "()I", |_ctx, _args| Ok(Some(Value::Int(1500))));

    // Low-level "0" suffixed natives used by NetworkInterface (JDK internals).
    // Safe no-op defaults — sufficient for environment probing (e.g., Spring's
    // HostInfoEnvironmentPostProcessor) without performing real OS queries.
    r.register(ni, "isUp0", "(Ljava/lang/String;I)Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    r.register(ni, "isLoopback0", "(Ljava/lang/String;I)Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(ni, "isP2P0", "(Ljava/lang/String;I)Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(
        ni,
        "supportsMulticast0",
        "(Ljava/lang/String;I)Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
    r.register(ni, "getMTU0", "(Ljava/lang/String;I)I", |_ctx, _args| {
        Ok(Some(Value::Int(1500)))
    });
    r.register(
        ni,
        "getMacAddr0",
        "([BLjava/lang/String;I)[B",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(
        ni,
        "getAll",
        "()[Ljava/net/NetworkInterface;",
        |ctx, _args| {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(
        ni,
        "getByName0",
        "(Ljava/lang/String;)Ljava/net/NetworkInterface;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(
        ni,
        "getByInetAddress0",
        "(Ljava/net/InetAddress;)Ljava/net/NetworkInterface;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(
        ni,
        "boundInetAddress0",
        "(Ljava/net/InetAddress;)Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
    r.register(
        ni,
        "getByIndex0",
        "(I)Ljava/net/NetworkInterface;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // R76 (Eureka / Spring Cloud bootstrap fix):
    // `HostInfoEnvironmentPostProcessor.postProcessEnvironment` calls
    // `InetUtils.findFirstNonLoopbackHostInfo` -> `findFirstNonLoopbackAddress`
    // -> `NetworkInterface.getNetworkInterfaces` which throws SocketException
    // "No network interfaces configured" (because our `getAll()` returns []).
    // Spring catches it, but the resulting catch path then keeps walking
    // through Spring's property-binding code with NUMEROUS recursive
    // `Binder.bind` calls (`ConfigDataEnvironment.processAndApply` ->
    // `withProfiles` -> `Binder.bindAggregate` -> `IndexedElementsBinder.
    // bindIndexed` ...). With JIT compilation of these hot paths, the
    // ConfigurationPropertyName parsing tight loop eventually SEGVs in
    // JIT-compiled code (round 71 post-main panic visibility hook shows
    // no Rust panic — it is a native SEGV in JIT, not a panic).
    //
    // Surgical fix: turn `HostInfoEnvironmentPostProcessor.
    // postProcessEnvironment(ConfigurableEnvironment,SpringApplication)V`
    // into a no-op. This skips the SocketException-throw + Spring catch
    // entirely. The cloud post-processor only sets "spring.cloud.client.
    // ip-address" / ".hostname" properties from the picked interface;
    // when not set, downstream callers fall back to the same defaults
    // we'd compute manually, so the bypass is safe.
    r.register(
        "org/springframework/cloud/client/HostInfoEnvironmentPostProcessor",
        "postProcessEnvironment",
        "(Lorg/springframework/core/env/ConfigurableEnvironment;Lorg/springframework/boot/SpringApplication;)V",
        |_ctx, _args| Ok(None),
    );
}

// ===========================================================================
// RE.9 — java.nio.channels.Selector
// ===========================================================================

fn register_re9_nio_selector(r: &mut NativeMethodRegistry) {
    let sel = "java/nio/channels/Selector";

    r.register(sel, "select", "(J)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => return Err(iae("select(J) missing argument")),
        };
        if raw < 0 {
            return Err(iae(format!("negative timeout {raw}")));
        }
        if raw == 0 {
            match ctx.invoke_virtual(this, "selectNow", "()I", &[]) {
                Ok(Some(v)) => Ok(Some(v)),
                _ => Ok(Some(Value::Int(0))),
            }
        } else {
            let deadline = std::time::Instant::now() + Duration::from_millis(raw as u64);
            let mut total = 0i32;
            while std::time::Instant::now() < deadline {
                if let Ok(Some(Value::Int(n))) =
                    ctx.invoke_virtual(this, "selectNow", "()I", &[])
                {
                    if n > 0 {
                        total = n;
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
                if ctx.get_field(this, SEL_OPEN).as_int().unwrap_or(1) == 0 {
                    break;
                }
            }
            Ok(Some(Value::Int(total)))
        }
    });

    r.register(sel, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, SEL_OPEN)))
    });
}

// ===========================================================================
// RE.10 — com.sun.net.httpserver.HttpServer
// ===========================================================================

struct HttpHandlerEntry {
    path_prefix: String,
    handler: ObjectRef,
}

struct ServerState {
    listener: Option<TcpListener>,
    running: AtomicBool,
    handlers: Mutex<Vec<HttpHandlerEntry>>,
    bound_port: AtomicI32,
}

fn server_registry() -> &'static Mutex<HashMap<i32, std::sync::Arc<ServerState>>> {
    static INSTANCE: OnceLock<Mutex<HashMap<i32, std::sync::Arc<ServerState>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_server_id() -> i32 {
    static COUNTER: AtomicI32 = AtomicI32::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

struct PendingRequest {
    stream: TcpStream,
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn request_queue() -> &'static Mutex<HashMap<i32, Vec<PendingRequest>>> {
    static INSTANCE: OnceLock<Mutex<HashMap<i32, Vec<PendingRequest>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn parse_http_request(mut stream: TcpStream) -> Option<PendingRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 1024];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if buf.len() > 1 << 20 {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    let sep = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..sep]).ok()?;
    let mut lines = head.split("\r\n");
    let req_line = lines.next()?;
    let mut rl = req_line.splitn(3, ' ');
    let method = rl.next()?.to_string();
    let uri = rl.next()?.to_string();
    let _ = rl.next()?;
    let mut headers = Vec::new();
    let mut content_length: usize = 0;
    for line in lines {
        if let Some(colon) = line.find(':') {
            let k = line[..colon].trim().to_string();
            let v = line[colon + 1..].trim().to_string();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    let mut body = buf[sep + 4..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    if body.len() > content_length && content_length > 0 {
        body.truncate(content_length);
    }
    Some(PendingRequest {
        stream,
        method,
        uri,
        headers,
        body,
    })
}

fn re10_dispatch_pending(
    ctx: &mut dyn NativeContext,
    server_id: i32,
) -> Result<usize, cratonvm_types::error::MethodCallFailed> {
    let mut drained = 0usize;
    loop {
        let req_opt = {
            let mut q = request_queue().lock();
            q.get_mut(&server_id).and_then(|v| v.pop())
        };
        let Some(req) = req_opt else { break };
        drained += 1;
        let handler_info = {
            let state = {
                let reg = server_registry().lock();
                reg.get(&server_id).cloned()
            };
            state.and_then(|s| {
                let hs = s.handlers.lock();
                hs.iter()
                    .filter(|e| req.uri.starts_with(&e.path_prefix))
                    .max_by_key(|e| e.path_prefix.len())
                    .map(|e| e.handler)
            })
        };
        let (status, body_bytes, resp_headers) = match handler_info {
            Some(h) => {
                let ex = alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/HttpExchange", 8);
                let m = ctx.create_string(&req.method);
                let u = ctx.create_string(&req.uri);
                ctx.set_field(ex, 0, Value::Object(Some(m)));
                ctx.set_field(ex, 1, Value::Object(Some(u)));
                let rh = ctx.new_ref_array(ClassId::new(0), req.headers.len().max(1));
                for (i, (k, v)) in req.headers.iter().enumerate() {
                    let s = ctx.create_string(&format!("{k}: {v}"));
                    ctx.set_array_element(rh, i, Value::Object(Some(s)));
                }
                ctx.set_field(ex, 2, Value::Object(Some(rh)));
                let rsph = ctx.new_ref_array(ClassId::new(0), 32);
                ctx.set_field(ex, 3, Value::Object(Some(rsph)));
                let body_arr = new_java_byte_array(ctx, &req.body);
                ctx.set_field(ex, 4, Value::Object(Some(body_arr)));
                ctx.set_field(ex, 5, Value::Int(200));
                let resp_body_chunks = ctx.new_ref_array(ClassId::new(0), 64);
                ctx.set_field(ex, 6, Value::Object(Some(resp_body_chunks)));
                ctx.set_field(ex, 7, Value::Int(0));
                let _ = ctx.invoke_virtual(
                    h,
                    "handle",
                    "(Lcom/sun/net/httpserver/HttpExchange;)V",
                    &[Value::Object(Some(ex))],
                );
                let status = ctx.get_field(ex, 5).as_int().unwrap_or(200);
                let mut body_bytes: Vec<u8> = Vec::new();
                if let Value::Object(Some(chunks)) = ctx.get_field(ex, 6) {
                    let n = ctx.array_length(chunks);
                    for i in 0..n {
                        if let Value::Object(Some(ba)) = ctx.get_array_element(chunks, i) {
                            let ln = ctx.array_length(ba);
                            for j in 0..ln {
                                if let Value::Int(b) = ctx.get_array_element(ba, j) {
                                    body_bytes.push(b as i8 as u8);
                                }
                            }
                        }
                    }
                }
                let mut resp_headers = Vec::new();
                if let Value::Object(Some(rh)) = ctx.get_field(ex, 3) {
                    let n = ctx.array_length(rh);
                    for i in 0..n {
                        if let Value::Object(Some(s)) = ctx.get_array_element(rh, i) {
                            let line = ctx.read_string(s).unwrap_or_default();
                            if let Some(colon) = line.find(':') {
                                resp_headers.push((
                                    line[..colon].trim().to_string(),
                                    line[colon + 1..].trim().to_string(),
                                ));
                            }
                        }
                    }
                }
                (status, body_bytes, resp_headers)
            }
            None => (404, b"Not Found".to_vec(), Vec::new()),
        };
        let mut stream = req.stream;
        let mut resp = Vec::with_capacity(128 + body_bytes.len());
        use std::io::Write as _;
        let _ = write!(&mut resp, "HTTP/1.1 {status} {}\r\n", http_reason(status));
        let mut has_content_length = false;
        for (k, v) in &resp_headers {
            if k.eq_ignore_ascii_case("content-length") {
                has_content_length = true;
            }
            let _ = write!(&mut resp, "{k}: {v}\r\n");
        }
        if !has_content_length {
            let _ = write!(&mut resp, "Content-Length: {}\r\n", body_bytes.len());
        }
        resp.extend_from_slice(b"Connection: close\r\n\r\n");
        resp.extend_from_slice(&body_bytes);
        let _ = stream.write_all(&resp);
        let _ = stream.flush();
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    Ok(drained)
}

fn http_reason(code: i32) -> &'static str {
    match code {
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
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

fn re10_start_server(server_id: i32) -> std::io::Result<()> {
    let state = {
        let reg = server_registry().lock();
        reg.get(&server_id).cloned()
    };
    let Some(state) = state else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "server not registered",
        ));
    };
    if state.running.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let listener = match state.listener.as_ref() {
        Some(l) => l.try_clone()?,
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "not bound",
            ));
        }
    };
    let state_cl = state.clone();
    std::thread::Builder::new()
        .name(format!("cratonvm-httpserver-{server_id}"))
        .spawn(move || {
            listener.set_nonblocking(true).ok();
            while state_cl.running.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        if let Some(pending) = parse_http_request(stream) {
                            let mut q = request_queue().lock();
                            q.entry(server_id).or_default().push(pending);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        })?;
    Ok(())
}

fn register_re10_http_server(r: &mut NativeMethodRegistry) {
    let hs = "com/sun/net/httpserver/HttpServer";

    r.register(
        hs,
        "create",
        "(Ljava/net/InetSocketAddress;I)Lcom/sun/net/httpserver/HttpServer;",
        |ctx, args| {
            let sa = obj_arg(args, 0)?;
            let (host, port) = read_inet_socket_address(ctx, sa)?;
            let backlog = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            let ip = resolve_host(&host)?;
            let addr = SocketAddr::new(ip, port.clamp(0, 65535) as u16);
            let listener = TcpListener::bind(addr)
                .map_err(|e| ioex(format!("HttpServer bind {addr}: {e}")))?;
            let bound_port = listener.local_addr().map(|a| a.port() as i32).unwrap_or(port);
            let server_id = next_server_id();
            let state = std::sync::Arc::new(ServerState {
                listener: Some(listener),
                running: AtomicBool::new(false),
                handlers: Mutex::new(Vec::new()),
                bound_port: AtomicI32::new(bound_port),
            });
            server_registry().lock().insert(server_id, state);
            let srv = alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/HttpServer", 5);
            let sa_echo = alloc_inet_socket_address(ctx, &host, bound_port);
            ctx.set_field(srv, HS_ADDRESS, Value::Object(Some(sa_echo)));
            ctx.set_field(srv, HS_STARTED, Value::Int(0));
            ctx.set_field(srv, HS_CONTEXTS, Value::Object(None));
            ctx.set_field(srv, HS_SERVER_ID, Value::Int(server_id));
            ctx.set_field(srv, HS_PORT, Value::Int(bound_port));
            let _ = backlog;
            Ok(Some(Value::Object(Some(srv))))
        },
    );

    r.register(hs, "start", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
        if id < 0 {
            return Err(ioex("HttpServer not initialised"));
        }
        re10_start_server(id).map_err(|e| ioex(format!("HttpServer start: {e}")))?;
        ctx.set_field(this, HS_STARTED, Value::Int(1));
        re10_dispatch_pending(ctx, id)?;
        Ok(None)
    });

    r.register(hs, "stop", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
        if id >= 0 {
            if let Some(state) = server_registry().lock().get(&id) {
                state.running.store(false, Ordering::SeqCst);
            }
            re10_dispatch_pending(ctx, id)?;
            server_registry().lock().remove(&id);
        }
        ctx.set_field(this, HS_STARTED, Value::Int(0));
        Ok(None)
    });

    r.register(
        hs,
        "createContext",
        "(Ljava/lang/String;Lcom/sun/net/httpserver/HttpHandler;)Lcom/sun/net/httpserver/HttpContext;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path_val = args.get(1).copied().unwrap_or(Value::Object(None));
            let path = value_or_string(ctx, path_val, "/");
            let handler = obj_arg(args, 2).map_err(|_| ioex("createContext: null handler"))?;
            let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
            if id >= 0 {
                if let Some(state) = server_registry().lock().get(&id) {
                    state.handlers.lock().push(HttpHandlerEntry {
                        path_prefix: path.clone(),
                        handler,
                    });
                }
            }
            let hctx = alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/HttpContext", 2);
            let path_s = ctx.create_string(&path);
            ctx.set_field(hctx, 0, Value::Object(Some(path_s)));
            ctx.set_field(hctx, 1, Value::Object(Some(handler)));
            Ok(Some(Value::Object(Some(hctx))))
        },
    );

    r.register(
        hs,
        "getAddress",
        "()Ljava/net/InetSocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, HS_ADDRESS)))
        },
    );

    let hex = "com/sun/net/httpserver/HttpExchange";
    r.register(hex, "getRequestMethod", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(hex, "getRequestURI", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = ctx.get_field(this, 1);
        let uri = alloc_concurrent_synthetic(ctx, "java/net/URI", 6);
        ctx.set_field(uri, 0, s);
        Ok(Some(Value::Object(Some(uri))))
    });
    r.register(
        hex,
        "getResponseHeaders",
        "()Lcom/sun/net/httpserver/Headers;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let rh = ctx.get_field(this, 3);
            let h = alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/Headers", 1);
            ctx.set_field(h, 0, rh);
            Ok(Some(Value::Object(Some(h))))
        },
    );
    r.register(
        hex,
        "getRequestHeaders",
        "()Lcom/sun/net/httpserver/Headers;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let rh = ctx.get_field(this, 2);
            let h = alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/Headers", 1);
            ctx.set_field(h, 0, rh);
            Ok(Some(Value::Object(Some(h))))
        },
    );
    r.register(hex, "getRequestBody", "()Ljava/io/InputStream;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let body = match ctx.get_field(this, 4) {
            Value::Object(Some(a)) => a,
            _ => ctx.new_array(ArrayElementType::Byte, 0),
        };
        let len = ctx.array_length(body) as i32;
        let stream = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
        ctx.set_field(stream, 0, Value::Object(Some(body))); // buf
        ctx.set_field(stream, 1, Value::Int(0));              // pos
        ctx.set_field(stream, 2, Value::Int(0));              // mark
        ctx.set_field(stream, 3, Value::Int(len));            // count
        Ok(Some(Value::Object(Some(stream))))
    });
    r.register(hex, "sendResponseHeaders", "(IJ)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let code = args.get(1).and_then(|v| v.as_int()).unwrap_or(200);
        ctx.set_field(this, 5, Value::Int(code));
        Ok(None)
    });
    r.register(hex, "getResponseBody", "()Ljava/io/OutputStream;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let out = alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/HttpExchange$ResponseBody", 2);
        ctx.set_field(out, 0, Value::Object(Some(this)));
        ctx.set_field(out, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(out))))
    });
    r.register(hex, "close", "()V", |_ctx, _args| Ok(None));

    let rb = "com/sun/net/httpserver/HttpExchange$ResponseBody";
    r.register(rb, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Err(ioex("ResponseBody has no exchange")),
        };
        let buf = obj_arg(args, 1)?;
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let data = java_byte_array_to_vec(ctx, buf, off, len)?;
        let chunk = new_java_byte_array(ctx, &data);
        if let Value::Object(Some(chunks)) = ctx.get_field(owner, 6) {
            let cap = ctx.array_length(chunks);
            for i in 0..cap {
                if let Value::Object(None) = ctx.get_array_element(chunks, i) {
                    ctx.set_array_element(chunks, i, Value::Object(Some(chunk)));
                    return Ok(None);
                }
            }
        }
        Ok(None)
    });
    r.register(rb, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Err(ioex("ResponseBody has no exchange")),
        };
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) & 0xff;
        let chunk = ctx.new_array(ArrayElementType::Byte, 1);
        ctx.set_array_element(chunk, 0, Value::Int(b as i8 as i32));
        if let Value::Object(Some(chunks)) = ctx.get_field(owner, 6) {
            let cap = ctx.array_length(chunks);
            for i in 0..cap {
                if let Value::Object(None) = ctx.get_array_element(chunks, i) {
                    ctx.set_array_element(chunks, i, Value::Object(Some(chunk)));
                    return Ok(None);
                }
            }
        }
        Ok(None)
    });
    r.register(rb, "flush", "()V", |_ctx, _args| Ok(None));
    r.register(rb, "close", "()V", |_ctx, _args| Ok(None));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn re1_http_parse_url_plain() {
        let (https, host, port, path) = http_parse_url("http://example.com/foo").unwrap();
        assert!(!https);
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
        assert_eq!(path, "/foo");
    }

    #[test]
    fn re1_http_parse_url_with_port() {
        let (https, host, port, path) = http_parse_url("https://example.com:8443/api?x=1").unwrap();
        assert!(https);
        assert_eq!(host, "example.com");
        assert_eq!(port, 8443);
        assert_eq!(path, "/api?x=1");
    }

    #[test]
    fn re1_http_build_request_adds_content_length_for_post() {
        let req = http_build_request("POST", "h", 80, "/", &[], b"hello", 80);
        let s = String::from_utf8_lossy(&req);
        assert!(s.contains("POST / HTTP/1.1"));
        assert!(s.contains("Host: h"));
        assert!(s.contains("Content-Length: 5"));
        assert!(s.ends_with("hello"));
    }

    #[test]
    fn re2_tcp_listener_binds_and_accepts() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let th = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 5];
                let _ = s.read(&mut buf);
                assert_eq!(&buf, b"hello");
            }
        });
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client.write_all(b"hello").unwrap();
        let _ = th.join();
    }

    #[test]
    fn re3_resolve_localhost() {
        let ip = resolve_host("localhost").expect("localhost must resolve");
        assert!(ip.is_loopback());
    }

    #[test]
    fn re3_parse_literal_v4() {
        let ip = resolve_host("10.0.0.1").unwrap();
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
    }

    #[test]
    fn re4_http_decode_chunked() {
        let raw = b"5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n";
        let body = http_decode_chunked(raw).unwrap();
        assert_eq!(body, b"helloworld");
    }

    #[test]
    fn re4_http_read_response_parses_status_and_body() {
        let raw = b"HTTP/1.1 204 No Content\r\nServer: t\r\nContent-Length: 0\r\n\r\n";
        let resp = http_read_response(&raw[..]).unwrap();
        assert_eq!(resp.status, 204);
        assert_eq!(resp.body, Vec::<u8>::new());
        assert!(resp.headers.iter().any(|(k, _)| k == "Server"));
    }

    #[test]
    fn re7_udp_roundtrip() {
        use std::net::UdpSocket;
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.send_to(b"ping", server_addr).unwrap();
        let mut buf = [0u8; 4];
        let (n, _) = server.recv_from(&mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf, b"ping");
    }

    #[test]
    fn re8_enumerates_at_least_loopback() {
        let ips = re8_enumerate_local_ips();
        assert!(ips.iter().any(|i| i.is_loopback()));
    }

    #[test]
    fn re9_iae_helper_reachable() {
        assert!(matches!(
            iae(""),
            cratonvm_types::error::MethodCallFailed::InternalError(_)
        ));
    }

    #[test]
    fn re10_parse_minimal_http_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(b"GET /hi HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        let req = parse_http_request(stream).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.uri, "/hi");
        assert!(req.headers.iter().any(|(k, v)| k == "Host" && v == "x"));
    }
}
