// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

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
    outer_jar_bytes_cache()
        .lock()
        .insert(path.to_string(), arc.clone());
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
    let mut entry = zip
        .by_name(inner_entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e.to_string()))?;
    let mut buf = Vec::with_capacity(entry.size().min(1 << 27) as usize);
    entry
        .read_to_end(&mut buf)
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
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};

use crate::servlet::{s2_alloc_listener, s2_alloc_stream, s2_registry};
use crate::{alloc_concurrent_synthetic, obj_arg};

// ---------------------------------------------------------------------------
// Synthetic field layouts used by Phase E.
// ---------------------------------------------------------------------------

const IA_HOST: usize = 0;
const IA_ADDR: usize = 1;

const ISA_HOST: usize = 0;
const ISA_PORT: usize = 1;

/// The list `SSLSocket.getSupportedCipherSuites()` returns for our synthetic
/// client socket (`ssl_sock_supported_cipher_suites` in phases_late.rs — kept
/// in sync manually since the two functions live in different files serving
/// different halves of the same `javax/net/ssl/SSLSocket` class). Used by
/// `setEnabledCipherSuites` below to distinguish a real cipher restriction
/// from a caller just re-asserting "use everything you support".
const CLIENT_SUPPORTED_CIPHER_SUITES: &[&str] = &[
    "TLS_AES_128_GCM_SHA256",
    "TLS_AES_256_GCM_SHA384",
    "TLS_CHACHA20_POLY1305_SHA256",
    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
    "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
];

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
    pub host_id: i32, // unused (we still keep `SOCK_HOST` in field for getInetAddress)
    pub port: i32,
    pub local_port: i32,
    pub closed: i32,
    pub stream_id: i32,
    /// Java's SO_TIMEOUT setting. Unlike the TCP stream id, this is meaningful
    /// before connect and must survive the stream installation transition.
    pub read_timeout_ms: i32,
    pub input_shutdown: i32,
    pub output_shutdown: i32,
}

#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct SsSide {
    pub port: i32,
    pub backlog: i32,
    pub closed: i32,
    pub listener_id: i32,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct NativeObjKey {
    vm: usize,
    identity: i32,
}

fn native_obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> NativeObjKey {
    NativeObjKey {
        vm: ctx.vm_identity(),
        identity: ctx.identity_hash_code(obj),
    }
}

// GC note (gc-followups-20260706): Socket state used to be keyed by the raw
// ObjectRef address. A moving GC can relocate a connected Socket between
// connect()/accept() and a later getInputStream()/getOutputStream(), making the
// lookup miss and default to stream_id=-1 ("not connected"). Key Socket state
// by VM identity + System.identityHashCode instead; both are stable across
// relocation, and the payload contains only plain integers. ServerSocket /
// DatagramSocket / InetAddress side tables still need the same treatment.
fn sock_side_table() -> &'static Mutex<HashMap<NativeObjKey, SockSide>> {
    static T: OnceLock<Mutex<HashMap<NativeObjKey, SockSide>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ss_side_table() -> &'static Mutex<HashMap<i32, SsSide>> {
    static T: OnceLock<Mutex<HashMap<i32, SsSide>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn sock_default() -> SockSide {
    SockSide {
        host_id: 0,
        port: 0,
        local_port: 0,
        closed: 0,
        stream_id: -1,
        read_timeout_ms: 0,
        input_shutdown: 0,
        output_shutdown: 0,
    }
}

fn sock_get(ctx: &dyn NativeContext, this: ObjectRef) -> SockSide {
    let t = sock_side_table().lock();
    t.get(&native_obj_key(ctx, this))
        .copied()
        .unwrap_or_else(sock_default)
}

fn sock_set<F: FnOnce(&mut SockSide)>(ctx: &dyn NativeContext, this: ObjectRef, f: F) {
    let mut t = sock_side_table().lock();
    let entry = t
        .entry(native_obj_key(ctx, this))
        .or_insert_with(sock_default);
    f(entry);
}

/// FIX (client-cipher-restriction): read the `s2_registry`/rustls stream id
/// backing a `Socket`/`SSLSocket` object created through this module (-1 if
/// not connected/tracked). Used by `http_url_connection::perform` to route
/// HTTPS I/O through a socket obtained by up-calling a real, caller-installed
/// `SSLSocketFactory.createSocket` (instead of `perform`'s own internal
/// connection) when that factory might apply configuration — like cipher
/// restriction via `SSLSocket.setEnabledCipherSuites` — that only takes
/// effect through the real Java call chain.
pub(crate) fn sock_stream_id_for_upcall(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    sock_get(ctx, this).stream_id
}

/// FIX (netty-client-socket-write-after-close): companion to
/// [`sock_stream_id_for_upcall`] for `phases_late.rs`'s NEW-13
/// `javax/net/ssl/SSLSocket` stream/lifecycle natives (`getInputStream`,
/// `getOutputStream`, `close`, `isClosed`, `isConnected`). Those read/write
/// raw object fields (`NEW13_SOCK_TLSID`/`NEW13_SOCK_CLOSED`), which is fine
/// for a socket `new13_do_create_socket` built itself — but
/// `SSLSocketFactory.createSocket(String,int)` is ALSO registered here (this
/// module registers `register_phase_e_networking` after `register_p68_ssl`,
/// so this implementation wins for that exact (class,name,descriptor) key)
/// and builds the socket through the side table above, leaving those raw
/// fields at their default `Object(None)`. Real bytecode method resolution
/// then finds phases_late.rs's `getOutputStream`/`close`/etc — they're
/// registered on the concrete `javax/net/ssl/SSLSocket` class, a more
/// specific match than anything this module registers on the `java/net/
/// Socket` superclass — so those raw-field readers ran regardless of which
/// factory built the object, read the never-populated field, and treated
/// every write on a `createSocket(host,port)`-obtained socket as though the
/// stream had already been closed. These accessors let phases_late.rs fall
/// back to the side table when the raw field isn't a valid entry.
pub(crate) fn sock_mark_closed_for_upcall(ctx: &dyn NativeContext, this: ObjectRef) {
    sock_set(ctx, this, |s| {
        s.closed = 1;
        s.stream_id = -1;
    });
}

pub(crate) fn sock_is_closed_for_upcall(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    sock_get(ctx, this).closed != 0
}

/// FIX (netty-client-socket-write-after-close): let `phases_late.rs`'s
/// `new13_do_create_socket` record its connect result HERE instead of (only)
/// in a raw object field. `alloc_concurrent_synthetic` sizes a "synthetic"
/// object using the REAL loaded class's actual field count/layout when the
/// class is loadable (`ctx.class_num_total_fields`) — see its own doc
/// comment — so field index 2 on a `javax/net/ssl/SSLSocket` lands wherever
/// the real class hierarchy's own 3rd field actually is, not a slot we
/// control. Traced with a same-native-call immediate readback
/// (CRATONVM_DBG_TLS_SOCK): `ctx.set_field(sock, 2, Value::Int(tls_id))`
/// followed instantly by `ctx.get_field(sock, 2)` — no Java code, no GC,
/// same call — already read back `Object(None)`, proving the object's real
/// field #2 is reference-typed and the GC/field-layout guard silently drops
/// a mismatched-type write rather than erroring. This is exactly the
/// collision this file's own `SockSide` table was introduced to avoid for
/// plain `java.net.Socket`/`ServerSocket`/`DatagramSocket`; `SSLSocket`
/// needs the same treatment.
pub(crate) fn sock_set_for_create(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    port: i32,
    stream_id: i32,
) {
    sock_set_for_create_with_local_port(ctx, this, port, 0, stream_id);
}

/// Same as [`sock_set_for_create`] but also records the real local (client)
/// port, for callers that already resolved it from the live `TcpStream`
/// (e.g. `phases_early.rs`'s `phase52_socket_connect`) instead of always
/// defaulting to 0.
pub(crate) fn sock_set_for_create_with_local_port(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    port: i32,
    local_port: i32,
    stream_id: i32,
) {
    sock_set(ctx, this, |s| {
        s.port = port;
        s.local_port = local_port;
        s.closed = 0;
        s.stream_id = stream_id;
    });
}

fn ss_get(ctx: &dyn NativeContext, this: ObjectRef) -> SsSide {
    let key = ctx.identity_hash_code(this);
    let t = ss_side_table().lock();
    t.get(&key).copied().unwrap_or(SsSide {
        port: -1,
        backlog: 50,
        closed: 0,
        listener_id: -1,
    })
}

fn ss_set<F: FnOnce(&mut SsSide)>(ctx: &dyn NativeContext, this: ObjectRef, f: F) {
    let key = ctx.identity_hash_code(this);
    let mut t = ss_side_table().lock();
    let entry = t.entry(key).or_insert(SsSide {
        port: -1,
        backlog: 50,
        closed: 0,
        listener_id: -1,
    });
    f(entry);
}

// `DatagramSocket` side-table — same rationale as `SockSide`/`SsSide` above.
// The synthetic natives stored port/closed/timeout/fd in object slots
// `DS_PORT=0 / DS_CLOSED=1 / DS_TIMEOUT=2 / DS_FD=3`, but in real-JDK mode the
// loaded `java.net.DatagramSocket` (JDK 17+) has a single instance field
// (`delegate`), so slots 1/2/3 are out of bounds — the writes were dropped by
// the GC guard, the `fd` was lost, and every send/receive saw a "closed"
// socket. Keying the state by `ObjectRef` makes it layout-independent. (The
// synthetic-JDK `register_p72_datagram` path keeps its 4-field-layout natives;
// only this real-JDK `register_re7_datagram_socket` set is converted.)
#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct DsSide {
    pub port: i32,    // local port (was DS_PORT)
    pub closed: i32,  // 0 = open, 1 = closed (was DS_CLOSED)
    pub timeout: i32, // SO_TIMEOUT ms (was DS_TIMEOUT)
    pub fd: i32,      // udp fd handle; -1 = closed/unset (was DS_FD)
}

fn ds_side_table() -> &'static Mutex<HashMap<ObjectRef, DsSide>> {
    static T: OnceLock<Mutex<HashMap<ObjectRef, DsSide>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ds_get(this: ObjectRef) -> DsSide {
    ds_side_table()
        .lock()
        .get(&this)
        .copied()
        .unwrap_or(DsSide {
            port: 0,
            closed: 0,
            timeout: 0,
            fd: -1,
        })
}

fn ds_set<F: FnOnce(&mut DsSide)>(this: ObjectRef, f: F) {
    let mut t = ds_side_table().lock();
    let entry = t.entry(this).or_insert(DsSide {
        port: 0,
        closed: 0,
        timeout: 0,
        fd: -1,
    });
    f(entry);
}

// Map Socket$SocketInputStream / Socket$SocketOutputStream synthetic
// instance -> owner Socket. The real-JDK inner classes have their own
// fields (`parent`, `in`/`out`); we cannot use raw slot indices safely.
//
// GC-safety fix: both `stream` and `owner` are raw `ObjectRef`s that a moving
// GC can relocate at any point between `getInputStream()`/`getOutputStream()`
// (which populate this table) and a much-later `read`/`write` call (which
// looks it up) — `is`/`os` are freshly-allocated, short-lived objects, prime
// young-gen relocation candidates. Keying by the raw `stream` pointer went
// stale the moment `is`/`os` moved (lookup miss -> spurious "has no owner"),
// and returning the raw `owner` pointer went stale the moment the Socket
// moved (a dispatch against the relocated, now-reclaimed address -> "Stale
// pointer detected ... falling back to CP class java/net/Socket"). Both
// reproduced via org.apache.catalina.realm.TestJNDIRealmIntegration once the
// GC-barrier accept-loop fix let the test run long enough to trigger a GC
// mid-stream. Fix: key by `identity_hash_code` (GC-stable) and hold the value
// as a global GC root (remapped by the collector on every move), resolved
// fresh at each read — the same pattern `register_var_handle_root`/
// `read_var_handle_root` use for the identical hazard on VarHandle statics.
fn stream_owner_table() -> &'static Mutex<HashMap<i32, usize>> {
    static T: OnceLock<Mutex<HashMap<i32, usize>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}
fn stream_owner_set(ctx: &mut dyn NativeContext, stream: ObjectRef, owner: ObjectRef) {
    let key = ctx.identity_hash_code(stream);
    let handle = ctx.add_global_root(owner);
    stream_owner_table().lock().insert(key, handle);
}
fn stream_owner_get(ctx: &mut dyn NativeContext, stream: ObjectRef) -> Option<ObjectRef> {
    let key = ctx.identity_hash_code(stream);
    let handle = *stream_owner_table().lock().get(&key)?;
    ctx.resolve_global_root(handle)
}

// InetAddress side-table — same rationale as the Socket / ServerSocket
// tables above. `java.net.InetAddress` is a real bootstrap class whose only
// instance fields are `holder` (a `java.net.InetAddress$InetAddressHolder`)
// and `holder6`/cached-lookup state — NOT plain `hostName` / `address`
// String fields. Writing the host/IP Strings straight into instance slots
// 0/1 puts a `String` in the `holder` slot; when a real-JDK `InetAddress`
// (or `Inet4Address`) method that we do not natively override runs, its
// bytecode does `getfield holder; invokevirtual InetAddressHolder.getXxx()`
// and the sub-`invokevirtual` retargets onto `java/lang/String`, raising a
// bogus `NoSuchMethodError java/lang/String.getHostName()`.
//
// Keeping host/IP in this ObjectRef-keyed table leaves the real-JDK
// instance slots at their zero-initialised (null) defaults, so any
// real-JDK InetAddress bytecode that does run sees the spec-correct
// "uninitialised holder" shape instead of a poisoned String.
fn inet_addr_side_table() -> &'static Mutex<HashMap<ObjectRef, (String, String)>> {
    static T: OnceLock<Mutex<HashMap<ObjectRef, (String, String)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record an InetAddress's `(hostName, ipAddress)` in the side table.
fn inet_addr_set(this: ObjectRef, host: &str, ip: &str) {
    inet_addr_side_table()
        .lock()
        .insert(this, (host.to_string(), ip.to_string()));
}

/// `pub(crate)` re-export of [`inet_addr_set`] for sibling modules that
/// allocate InetAddress mirrors (e.g. `inet_address.rs`'s real DNS resolver).
///
/// Prefer [`alloc_inet_address_external`] for fresh allocations — it also
/// populates the real-JDK `holder`. Use this bare setter only when the
/// `InetAddress` object already exists.
pub(crate) fn inet_addr_set_external(this: ObjectRef, host: &str, ip: &str) {
    inet_addr_set(this, host, ip);
}

/// `pub(crate)` allocation helper for sibling modules (`inet_address.rs`,
/// `phases_early.rs`, `phases_late.rs`) that need to mint an `InetAddress`
/// mirror. Routes through [`alloc_inet_address`] so every mirror gets BOTH
/// the side-table entry AND a real-JDK `InetAddress$InetAddressHolder` —
/// never a bare `String` written into the typed `holder` slot.
pub(crate) fn alloc_inet_address_external(
    ctx: &mut dyn NativeContext,
    host: &str,
    ip: &str,
) -> ObjectRef {
    alloc_inet_address(ctx, host, ip)
}

/// `pub(crate)` re-export of [`resolve_host`] for sibling modules that need
/// to resolve a hostname/literal to an IP string without duplicating the
/// IPv4/IPv6-literal-then-DNS-fallback logic (used by `phases_early.rs`'s
/// synthetic `InetSocketAddress(String,int)` constructor — see its call site
/// for why an unconditionally-unresolved address is wrong there).
pub(crate) fn resolve_host_external(host: &str) -> Option<String> {
    resolve_host(host).ok().map(|ip| ip.to_string())
}

/// Read an InetAddress's `(hostName, ipAddress)` from the side table.
/// Returns `None` for an InetAddress we never recorded.
///
/// `pub(crate)` so the duplicate InetAddress natives registered in
/// `phases_early.rs` can consult the same table — otherwise they would read
/// the (now intentionally unpopulated) instance slots and return null.
pub(crate) fn inet_addr_get(this: ObjectRef) -> Option<(String, String)> {
    inet_addr_side_table().lock().get(&this).cloned()
}

/// GC root scan for [`inet_addr_side_table`]. Same stale-pointer hazard this
/// pattern already fixes elsewhere in this file (`gc_scan_re10_handler_roots`)
/// and in `lib.rs` (`gc_scan_locale_roots`): the table is a process-global
/// `HashMap<ObjectRef, _>` keyed by the synthetic mirror's identity, invisible
/// to every other scan, so a moving young GC that relocates a live
/// `InetAddress` mirror leaves the table keyed on a vacated from-space slot —
/// `getHostAddress()`/`getAddress()`/`toString()` then silently miss the table
/// and fall through to the real-JDK `holder` fallback in [`inet_addr_resolve`],
/// which reports `0.0.0.0` instead of the mirror's real address (ES
/// `InetAddressRandomBinaryDocValuesRangeQueryTests` CONTAINS-query false
/// negative). Remap companion: [`gc_update_inet_addr_refs`].
pub fn gc_scan_inet_addr_roots(out: &mut Vec<ObjectRef>) {
    for k in inet_addr_side_table().lock().keys() {
        out.push(*k);
    }
}

/// Companion to [`gc_scan_inet_addr_roots`]: after a moving collection,
/// re-key the side table so lookups keyed on the OLD `ObjectRef` still
/// resolve — the mirror's identity is now the relocated address.
pub fn gc_update_inet_addr_refs(pointer_map: &std::collections::HashMap<usize, usize>) {
    if pointer_map.is_empty() {
        return;
    }
    let mut map = inet_addr_side_table().lock();
    let drained: Vec<_> = map.drain().collect();
    for (k, v) in drained {
        let nk = pointer_map
            .get(&(k.as_ptr() as usize))
            .map(|&n| unsafe { ObjectRef::from_raw(n as *mut u8) })
            .unwrap_or(k);
        map.insert(nk, v);
    }
}

/// Read an InetAddress's `(hostName, ipAddress)` resolving through every
/// known layout: ObjectRef-keyed side table first, then the real-JDK
/// `holder`/`holder6` reference fields (`InetAddress$InetAddressHolder.hostName`
/// + `.address`/`.family`, `Inet6Address$Inet6AddressHolder.ipaddress`), then
/// `None`.
///
/// This is the layout-aware reader the report calls for: an `InetAddress`
/// that was allocated by real-JDK `<init>` (not by `alloc_inet_address`) —
/// e.g. via the un-overridden `InetAddress.getByAddress(String, byte[])`
/// two-arg factory — still resolves correctly because `populate_inet_holder`
/// mirrors host/IP into the real `holder`/`holder6`. `pub(crate)` so sibling
/// modules' duplicate InetAddress natives consult the same path.
pub(crate) fn inet_addr_resolve(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<(String, String)> {
    if let Some(pair) = inet_addr_get(this) {
        return Some(pair);
    }
    // Real-JDK `holder` path. `InetAddress$InetAddressHolder` carries
    // `String hostName`, `int address`, `int family`.
    if let Value::Object(Some(holder)) = ctx.get_field_by_name(this, "holder") {
        let host = match ctx.get_field_by_name(holder, "hostName") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        };
        // A real-JDK `Inet6Address` stores its 16-byte address in a SEPARATE
        // `holder6` field (`Inet6Address$Inet6AddressHolder.ipaddress`); the
        // base `holder.address` int is always 0 for v6 (see
        // `populate_inet_holder`). Without checking `holder6` first, any
        // Inet6Address that reaches this fallback reads `address == 0` and
        // reports "0.0.0.0" instead of its real 16-byte value.
        let ip = match ctx.get_field_by_name(this, "holder6") {
            Value::Object(Some(holder6)) => match ctx.get_field_by_name(holder6, "ipaddress") {
                Value::Object(Some(arr)) if ctx.array_length(arr) == 16 => {
                    let mut octets = [0u8; 16];
                    for (i, o) in octets.iter_mut().enumerate() {
                        *o = match ctx.get_array_element(arr, i) {
                            Value::Int(b) => (b & 0xff) as u8,
                            _ => 0,
                        };
                    }
                    Some(hotspot_ip_string(
                        &std::net::Ipv6Addr::from(octets).to_string(),
                    ))
                }
                _ => None,
            },
            _ => match ctx.get_field_by_name(holder, "address") {
                Value::Int(packed) => {
                    // `InetAddressHolder.address` is the IPv4 address packed
                    // big-endian into an int (Inet4Address layout).
                    let b = (packed as u32).to_be_bytes();
                    Some(std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]).to_string())
                }
                _ => None,
            },
        };
        if host.is_some() || ip.is_some() {
            return Some((host.unwrap_or_default(), ip.unwrap_or_default()));
        }
    }
    None
}

/// IPv4 / IPv6 family discriminant matching real-JDK
/// `InetAddress.IPv4 == 1` / `InetAddress.IPv6 == 2`.
const IA_FAMILY_V4: i32 = 1;
const IA_FAMILY_V6: i32 = 2;

/// Populate a real-JDK `InetAddress$InetAddressHolder` into the `holder`
/// reference field of a CratonVM-synthesised `InetAddress` mirror.
///
/// The side table remains the source of truth for the natives we override,
/// but un-overridden real-JDK `InetAddress` / `Inet4Address` bytecode reads
/// state straight out of `this.holder` — e.g. the `final` accessor
/// `InetAddress.getHostName()` is `holder().getHostName()`. Leaving `holder`
/// null makes that bytecode NPE (or dispatch a method against a null
/// receiver, surfacing as the bogus `NoSuchMethodError
/// java/lang/Object.toLowerCase` Hazelcast's `DefaultAddressPicker` trips
/// when it calls `inetAddress.getHostName().toLowerCase(Locale)`).
///
/// Mirrors the `alloc_inet_socket_address` holder-population pattern.
fn populate_inet_holder(ctx: &mut dyn NativeContext, ia: ObjectRef, host: &str, ip: &str) {
    // Only populate if the class actually declares a `holder` field — i.e.
    // a real-JDK `InetAddress` is loaded. With a purely synthetic stub the
    // field is absent and `set_field_by_name` is a harmless no-op anyway.
    let holder = alloc_concurrent_synthetic(ctx, "java/net/InetAddress$InetAddressHolder", 3);
    let host_str = ctx.create_string(host);
    ctx.set_field_by_name(holder, "hostName", Value::Object(Some(host_str)));
    // `address` is the IPv4 address packed big-endian into an int; for IPv6
    // it stays 0 (the bytes live in the separate `Inet6Address` holder).
    let parsed = ip.parse::<std::net::IpAddr>();
    let (packed, family) = match parsed {
        Ok(std::net::IpAddr::V4(v4)) => (i32::from_be_bytes(v4.octets()), IA_FAMILY_V4),
        Ok(std::net::IpAddr::V6(_)) => (0, IA_FAMILY_V6),
        Err(_) => (0, IA_FAMILY_V4),
    };
    ctx.set_field_by_name(holder, "address", Value::Int(packed));
    ctx.set_field_by_name(holder, "family", Value::Int(family));
    ctx.set_field_by_name(ia, "holder", Value::Object(Some(holder)));

    // NIO-SERVER-SOCKET (IPv6): a real-JDK `Inet6Address` stores its 16-byte
    // address in a SEPARATE `holder6` field
    // (`Inet6Address$Inet6AddressHolder { byte[16] ipaddress; int scope_id; …}`),
    // NOT in the base `holder` (whose `address` int is 0 for v6). Un-overridden
    // real-JDK Inet6Address bytecode — `isLinkLocalAddress()`, `getScopeId()`,
    // and the address checks `NioSocketImpl.bind`/`connect` run on the real
    // socket path — dereferences `holder6`; leaving it null NPEs before bind0
    // is ever reached. Populate it so the real path resolves v6 correctly.
    if let Ok(std::net::IpAddr::V6(v6)) = parsed {
        let h6 = alloc_concurrent_synthetic(ctx, "java/net/Inet6Address$Inet6AddressHolder", 5);
        let octets = v6.octets();
        let arr = ctx.new_array(ArrayElementType::Byte, octets.len());
        for (i, b) in octets.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i32));
        }
        ctx.set_field_by_name(h6, "ipaddress", Value::Object(Some(arr)));
        // Loopback / global addresses carry no scope; link-local scope ids are
        // not recoverable from a bare `Ipv6Addr`, so leave scope_id unset (0).
        ctx.set_field_by_name(h6, "scope_id", Value::Int(0));
        ctx.set_field_by_name(h6, "scope_id_set", Value::Int(0));
        ctx.set_field_by_name(ia, "holder6", Value::Object(Some(h6)));
    }
}

/// Read one logical InetAddress field (`IA_HOST` or `IA_ADDR`) — side table
/// first, then the real-JDK `holder` reference field, finally falling back
/// to the legacy synthetic instance slot for any InetAddress object not
/// built by `alloc_inet_address`.
fn inet_addr_field(ctx: &mut dyn NativeContext, this: ObjectRef, which: usize) -> Value {
    if let Some((host, ip)) = inet_addr_resolve(ctx, this) {
        let s = if which == IA_HOST { host } else { ip };
        return Value::Object(Some(ctx.create_string(&s)));
    }
    ctx.get_field(this, which)
}

/// String form of one logical InetAddress field, with a default for the
/// missing/empty case. Mirrors `read_field_string_or` but consults the
/// side table and the real-JDK `holder` first.
fn inet_addr_field_string_or(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    which: usize,
    default: &str,
) -> String {
    if let Some((host, ip)) = inet_addr_resolve(ctx, this) {
        let s = if which == IA_HOST { host } else { ip };
        return if s.is_empty() { default.to_string() } else { s };
    }
    read_field_string_or(ctx, this, which, default)
}

const SEL_OPEN: usize = 0;

const HS_ADDRESS: usize = 0;
const HS_STARTED: usize = 1;
const HS_CONTEXTS: usize = 2;
const HS_SERVER_ID: usize = 3;
const HS_PORT: usize = 4;
const HS_IMPL_CLASS: &str = "sun/net/httpserver/HttpServerImpl";
/// Executor supplied through `HttpServer.setExecutor`. It is retained so the
/// public `getExecutor` contract is coherent even though the native server's
/// VM dispatcher owns the actual request-draining threads.
const HS_EXECUTOR: usize = 5;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ioex<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IOException {
        message: message.into(),
    }
    .into()
}

/// Throw the concrete `java.net.UnknownHostException` (a subclass of
/// IOException). Code that catches `UnknownHostException` specifically (e.g.
/// Tomcat `NetMask`) misses a bare IOException, so host-resolution failures
/// must use this rather than `ioex("UnknownHostException: ...")`.
fn uhex<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::UnknownHostException {
        message: message.into(),
    }
    .into()
}
/// A missing jar/zip entry must surface as `java.io.FileNotFoundException`
/// (a subclass of `IOException`), matching the real JDK's
/// `JarURLConnection.getInputStream()` contract. Callers such as SmallRye
/// Config rely on `catch (FileNotFoundException)` to *skip* an absent
/// profile-specific resource; a plain `IOException` is rethrown and aborts boot.
fn fnfex<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::FileNotFoundException {
        path: message.into(),
    }
    .into()
}
/// Map a `zip::ZipArchive::by_name` failure to the right Java exception:
/// `ZipError::FileNotFound` → `FileNotFoundException`, anything else → `IOException`.
fn zip_entry_err(
    entry: &str,
    container: &str,
    e: zip::result::ZipError,
) -> cratonvm_types::error::MethodCallFailed {
    match e {
        zip::result::ZipError::FileNotFound => fnfex(format!(
            "entry {entry} in {container}: specified file not found in archive"
        )),
        other => ioex(format!(
            "URL.openStream: entry {entry} in {container}: {other}"
        )),
    }
}
fn npe<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::NullPointerException {
        message: Some(message.into()),
    }
    .into()
}
fn iae<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: message.into(),
    }
    .into()
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

/// If `url` carries a non-null, *application-provided* `URLStreamHandler`,
/// invoke its `openConnection(URL)` and return the resulting `URLConnection`.
///
/// This mirrors the real `java.net.URL.openConnection()` = `handler
/// .openConnection(this)` for schemes CratonVM does not resolve natively. The
/// canonical user is ShrinkWrap's in-memory `archive:` handler
/// (`ShrinkWrapClassLoader`), whose connection reads resources straight out of a
/// heap-resident `JavaArchive` — there is no `file:`/`jar:` path to fall back
/// to. `URL`s that CratonVM synthesises (`build_synthetic_url`,
/// `alloc_concurrent_synthetic`) never populate `handler`, and JDK built-in
/// handlers live under `sun.net.www.protocol.*`; both are excluded so this only
/// fires for genuinely app-supplied handlers.
///
/// Returns:
///   * `Ok(Some(conn))` — a custom handler is present; `conn` is its
///     `URLConnection` value (possibly `Value::Object(None)` if the handler
///     itself returned null).
///   * `Ok(None)` — no custom handler; the caller should use its own
///     (string-based) resolution path.
///   * `Err(e)` — the handler threw (propagate; e.g. `FileNotFoundException`).
pub(crate) fn url_custom_handler_connection(
    ctx: &mut dyn NativeContext,
    url: ObjectRef,
) -> Result<Option<Value>, cratonvm_types::error::MethodCallFailed> {
    let handler = match ctx.get_field_by_name(url, "handler") {
        Value::Object(Some(h)) => h,
        _ => return Ok(None),
    };
    // Skip the JDK's built-in protocol handlers — those schemes (file:, jar:,
    // http:, …) are resolved by the dedicated arms of `openStream`/
    // `openConnection`, and routing them back through the real handler would
    // re-enter the very natives this is a fallback for.
    let hclass = ctx
        .class_name_of_id(ctx.class_id_of_object(handler))
        .unwrap_or_default();
    if hclass.starts_with("sun/net/") {
        return Ok(None);
    }
    // `handler.openConnection(url)` — `invoke_virtual` prepends the receiver, so
    // `args` is just the URL. A null/absent handler method or a thrown
    // exception both propagate to the caller via `?`.
    let conn = ctx.invoke_virtual(
        handler,
        "openConnection",
        "(Ljava/net/URL;)Ljava/net/URLConnection;",
        &[Value::Object(Some(url))],
    )?;
    Ok(Some(conn.unwrap_or(Value::Object(None))))
}

/// Resolve the jar-file component of a `jar:...!/entry` URL when that
/// component is itself a Tomcat `war:file:<war-path>*/<entry-in-war>`
/// reference (e.g. `jar:war:file:/x.war*/WEB-INF/lib/test.jar!/entry` — a jar
/// packaged inside a WAR). Returns the nested jar's raw bytes read out of the
/// enclosing WAR's zip central directory, or `None` if `jar_raw` isn't a
/// `war:` reference (the plain on-disk-file case is handled separately by
/// callers). Mirrors `UriUtil.warToJar`'s `*/` → `!/` translation, but reads
/// the referenced entry's bytes (via the existing nested-jar cache) instead
/// of just rewriting the URL string, since the entry lives inside another
/// archive rather than directly on disk.
fn war_nested_jar_bytes(jar_raw: &str) -> Option<Arc<Vec<u8>>> {
    let rest = jar_raw.strip_prefix("war:")?.trim_start_matches("file:");
    let mut wparts = rest.splitn(2, "*/");
    let war_path_raw = wparts.next()?;
    let inner_entry = wparts.next()?;
    if inner_entry.is_empty() {
        return None;
    }
    // `file:` URLs prefix a leading `/` before a Windows drive letter
    // (`/C:/…`); try the trimmed form first, then the raw form for POSIX.
    let trimmed = war_path_raw.trim_start_matches('/');
    let war_disk = if std::path::Path::new(trimmed).exists() {
        trimmed.to_string()
    } else if std::path::Path::new(war_path_raw).exists() {
        war_path_raw.to_string()
    } else {
        trimmed.to_string()
    };
    cached_nested_jar(&war_disk, inner_entry).ok()
}

/// Build a `java/util/jar/JarEntry` from an already-opened zip archive, or
/// `Value::Object(None)` if `lookup` isn't present. Shared by the plain
/// on-disk and WAR-nested-jar lookup paths in `jar_url_lookup_entry`.
fn jar_entry_value_from_archive<R: std::io::Read + std::io::Seek>(
    ctx: &mut dyn NativeContext,
    archive: &mut zip::ZipArchive<R>,
    lookup: &str,
) -> Value {
    let (name, size, csize, method) = match archive.by_name(lookup) {
        Ok(entry) => {
            let name = entry.name().to_string();
            let size = entry.size() as i64;
            let csize = entry.compressed_size() as i64;
            #[allow(deprecated)]
            let method = entry.compression().to_u16() as i32;
            (name, size, csize, method)
        }
        Err(_) => return Value::Object(None),
    };
    let je = alloc_concurrent_synthetic(ctx, "java/util/jar/JarEntry", 4);
    let name_s = ctx.create_string(&name);
    ctx.set_field(je, 0, Value::Object(Some(name_s)));
    ctx.set_field(je, 1, Value::Long(size));
    ctx.set_field(je, 2, Value::Long(csize));
    ctx.set_field(je, 3, Value::Int(method));
    Value::Object(Some(je))
}

/// Parse a `jar:[file:]<path>!/<entry>` external form and return the entry's
/// uncompressed size from the zip central directory, or `None` if the URL is
/// not a resolvable jar-entry URL. Used by `JarURLConnection.getContentLength*`.
///
/// `<path>` is usually a plain on-disk jar/zip file, but Tomcat's `war:`
/// nested-archive scheme (see `war_nested_jar_bytes`) produces
/// `war:file:<war-path>*/<entry-in-war>` here instead — a jar packaged inside
/// a WAR (e.g. `jar:war:file:/x.war*/WEB-INF/lib/test.jar!/META-INF/…`, from
/// `WarURLConnection` wrapping the jar it points into). Handle that case by
/// reading the nested jar's bytes out of the WAR first, then treating those
/// bytes as the archive to look the entry up in.
fn jar_url_entry_size(ext: &str) -> Option<i64> {
    let after = ext
        .strip_prefix("jar:file:")
        .or_else(|| ext.strip_prefix("jar:"))?;
    let mut parts = after.splitn(2, "!/");
    let jar_raw = parts.next()?.trim_start_matches("file:");
    let entry_name = parts.next()?;
    if entry_name.is_empty() {
        return None;
    }
    if let Some(bytes) = war_nested_jar_bytes(jar_raw) {
        let cursor = std::io::Cursor::new(bytes.as_slice());
        let mut archive = zip::ZipArchive::new(cursor).ok()?;
        let entry = archive.by_name(entry_name).ok()?;
        return Some(entry.size() as i64);
    }
    // `file:` URLs prefix a leading `/` before a Windows drive letter
    // (`/C:/…`); try the trimmed form first, then the raw form for POSIX.
    let trimmed = jar_raw.trim_start_matches('/');
    let disk = if std::path::Path::new(trimmed).exists() {
        trimmed.to_string()
    } else if std::path::Path::new(jar_raw).exists() {
        jar_raw.to_string()
    } else {
        trimmed.to_string()
    };
    let file = std::fs::File::open(&disk).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let lookup = jmod_zip_entry_name(&disk, entry_name);
    let entry = archive.by_name(&lookup).ok()?;
    Some(entry.size() as i64)
}

/// Recover the originating `jar:…!/entry` external form from a synthetic
/// `JarURLConnection` (URL stored in field `HUC_URL`).
fn jar_url_conn_ext(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    let url_obj = match ctx.get_field(this, HUC_URL) {
        Value::Object(Some(o)) => o,
        _ => return String::new(),
    };
    let mut ext = read_field_string_or(ctx, url_obj, 5, "");
    if !ext.starts_with("jar:") {
        if let Ok(Some(Value::Object(Some(o)))) =
            ctx.invoke_virtual(url_obj, "toExternalForm", "()Ljava/lang/String;", &[])
        {
            ext = ctx.read_string(o).unwrap_or_default();
        }
    }
    ext
}

fn file_url_path(url: &str) -> Option<String> {
    let raw = url.strip_prefix("file:")?;
    let without_host = raw.strip_prefix("//").unwrap_or(raw);
    let path = if cfg!(windows) {
        without_host.trim_start_matches('/').to_string()
    } else {
        without_host.to_string()
    };
    Some(path)
}

fn synthetic_resource_url_content_len(ctx: &mut dyn NativeContext, url: &str) -> i64 {
    if let Some(path) = file_url_path(url) {
        return std::fs::metadata(path)
            .map(|m| m.len() as i64)
            .unwrap_or(-1);
    }

    let resource = if let Some(rest) = url.strip_prefix("jrt:") {
        let path = rest.trim_start_matches('/');
        match path.split_once('/') {
            Some((_module, entry)) => entry,
            None => path,
        }
    } else if let Some(rest) = url
        .strip_prefix("classpath:")
        .or_else(|| url.strip_prefix("resource:"))
    {
        rest.trim_start_matches('/')
    } else {
        return -1;
    };

    ctx.find_resource(resource)
        .map(|bytes| bytes.len() as i64)
        .unwrap_or(-1)
}

fn synthetic_resource_url_last_modified(url: &str) -> i64 {
    let Some(path) = file_url_path(url) else {
        return 0;
    };
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    let Ok(modified) = meta.modified() else {
        return 0;
    };
    match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis().min(i64::MAX as u128) as i64,
        Err(_) => 0,
    }
}

/// `JarURLConnection.getJarEntry()` — build the `java/util/jar/JarEntry` for the
/// entry named in the `jar:…!/entry` URL by reading the zip central directory.
/// Returns `Value::Object(None)` when the URL has no entry or the jar/entry is
/// missing. Spring's `AbstractFileResolvingResource.checkReadable()` reads this
/// (then `JarEntry.isDirectory()`) for jar resources — not `getContentLength`.
/// `.jmod` archives store their class/resource entries under a `classes/`
/// prefix, but a `getResource` URL into a jmod omits it (e.g.
/// `…/java.base.jmod!/java/lang/Object.class`). Map the requested entry to its
/// real on-disk zip-entry name, mirroring `URL.openStream` (line ~3931) and
/// `find_resource`. Non-jmod jars and already-prefixed names pass through.
///
/// Without this, `JarURLConnection.getJarEntry()`/`getContentLength()` on a
/// jmod URL (the form CratonVM hands back from `getResource` for a JDK
/// runtime class — HotSpot uses `jrt:` instead) found nothing, so Spring's
/// `AbstractFileResolvingResource.checkReadable()` jar branch
/// (`getJarEntry() != null`) reported a perfectly readable JDK class resource
/// as NOT readable (ModuleResourceTests.existingClassFileResource —
/// `ClassPathResource("java/beans/Introspector.class").isReadable()`).
fn jmod_zip_entry_name<'a>(disk: &str, entry: &'a str) -> std::borrow::Cow<'a, str> {
    if disk.ends_with(".jmod") && !entry.starts_with("classes/") {
        std::borrow::Cow::Owned(format!("classes/{entry}"))
    } else {
        std::borrow::Cow::Borrowed(entry)
    }
}

fn jar_url_lookup_entry(ctx: &mut dyn NativeContext, ext: &str) -> Value {
    let after = match ext
        .strip_prefix("jar:file:")
        .or_else(|| ext.strip_prefix("jar:"))
    {
        Some(a) => a,
        None => return Value::Object(None),
    };
    let mut parts = after.splitn(2, "!/");
    let jar_raw = match parts.next() {
        Some(p) => p.trim_start_matches("file:"),
        None => return Value::Object(None),
    };
    let entry_name = match parts.next() {
        Some(e) if !e.is_empty() => e,
        _ => return Value::Object(None),
    };
    // Tomcat `war:` nested-jar case (see `war_nested_jar_bytes`): the jar
    // component is packaged inside a WAR rather than sitting directly on
    // disk, so its bytes must come from the enclosing WAR's zip entry.
    if let Some(bytes) = war_nested_jar_bytes(jar_raw) {
        let cursor = std::io::Cursor::new(bytes.as_slice());
        let mut archive = match zip::ZipArchive::new(cursor) {
            Ok(a) => a,
            Err(_) => return Value::Object(None),
        };
        return jar_entry_value_from_archive(ctx, &mut archive, entry_name);
    }
    let trimmed = jar_raw.trim_start_matches('/');
    let disk = if std::path::Path::new(trimmed).exists() {
        trimmed.to_string()
    } else if std::path::Path::new(jar_raw).exists() {
        jar_raw.to_string()
    } else {
        trimmed.to_string()
    };
    let file = match std::fs::File::open(&disk) {
        Ok(f) => f,
        Err(_) => return Value::Object(None),
    };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(_) => return Value::Object(None),
    };
    // Extract the entry metadata into owned values, then drop the `archive`
    // borrow before doing any `ctx` allocation (mirrors p59_jar_collect_entries).
    let lookup = jmod_zip_entry_name(&disk, entry_name);
    jar_entry_value_from_archive(ctx, &mut archive, &lookup)
}

/// Recover the originating `jar:…!/entry` URL from a synthetic
/// `JarURLConnection` (field `HUC_URL`) and return the jar entry's size, or
/// `-1` when it can't be resolved (matching `URLConnection` semantics).
fn jar_url_conn_entry_size(ctx: &mut dyn NativeContext, this: ObjectRef) -> i64 {
    let ext = jar_url_conn_ext(ctx, this);
    jar_url_entry_size(&ext).unwrap_or(-1)
}

pub(crate) fn read_inet_socket_address(
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
                        // CratonVM-synthesised InetAddress: host/IP live in the
                        // ObjectRef-keyed side table (instance slots are the
                        // real-JDK `holder` reference fields — see
                        // `inet_addr_side_table`).
                        if let Some((host, ip)) = inet_addr_get(addr_obj) {
                            if !host.is_empty() {
                                resolved = Some(host);
                            } else if !ip.is_empty() {
                                resolved = Some(ip);
                            }
                        }
                        // Legacy synthetic InetAddress: slot 0 = hostName
                        // String, slot 1 = ip String (kept for objects not
                        // built by `alloc_inet_address`).
                        if resolved.is_none() {
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
                        }
                        // real-JDK InetAddress: slot 0 = InetAddressHolder; the
                        // holder's slot 0 is hostName, slot 1 packs address bytes.
                        if resolved.is_none() {
                            if let Value::Object(Some(inner_holder)) = ctx.get_field(addr_obj, 0) {
                                if let Value::Object(Some(name_obj)) =
                                    ctx.get_field(inner_holder, 0)
                                {
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
    // Port: a real-JDK `InetSocketAddress` declares a single `holder` field at
    // slot 0; the port lives at `holder.port` (slot 2). Its slot 1 (our legacy
    // `ISA_PORT`) is therefore out of the declared layout and reads back as a
    // stale `Int(0)` — which the previous "read ISA_PORT first" logic accepted,
    // so `Socket.connect(new InetSocketAddress(host, port))` targeted port 0
    // (`ConnectException: host:0`). Mirror the host branch above: when the
    // slot-0 value is a real `InetSocketAddressHolder` object (i.e. not a
    // String, which is the legacy-synthetic layout), read the port from
    // `holder.port`; only fall back to the direct `ISA_PORT` slot for the
    // legacy synthetic layout (host String at slot 0, port int at slot 1).
    let port = match holder_val {
        Value::Object(Some(holder)) if ctx.read_string(holder).is_none() => {
            match ctx.get_field(holder, 2) {
                Value::Int(n) => n,
                Value::Long(n) => n as i32,
                // Holder carried no int port — fall back to the direct slot.
                _ => match ctx.get_field(sa, ISA_PORT) {
                    Value::Int(n) => n,
                    Value::Long(n) => n as i32,
                    _ => 0,
                },
            }
        }
        _ => match ctx.get_field(sa, ISA_PORT) {
            Value::Int(n) => n,
            Value::Long(n) => n as i32,
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

fn socket_dbg_bytes(data: &[u8]) -> String {
    let preview_len = data.len().min(32);
    let mut preview = String::new();
    for (idx, byte) in data.iter().take(preview_len).enumerate() {
        if idx != 0 {
            preview.push(' ');
        }
        preview.push_str(&format!("{byte:02x}"));
    }
    if data.len() > preview_len {
        preview.push_str(" ...");
    }
    preview
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

/// Canonicalise an IP address string into the exact textual form HotSpot's
/// `InetAddress` stores, so `getHostAddress()` / `toString()` are byte-identical:
///
///   * IPv4-mapped IPv6 (`::ffff:a.b.c.d`) is folded to its IPv4 dotted-quad,
///     matching HotSpot: `getByName` / `getByAddress` / the socket peer decoder
///     all hand back an `Inet4Address` (4-byte `getAddress()`) for a v4-mapped
///     address. Without the fold our mirror stays a 16-byte `Inet6Address`, so
///     any IPv4 CIDR test — e.g. Tomcat's `RemoteIpFilter` matching a dual-stack
///     loopback peer against `127.0.0.0/8` — silently fails on
///     `NetMask.matches`'s 4-vs-16 length guard.
///   * Genuine IPv6 is rendered in HotSpot's FULL eight-group form
///     (`Inet6Address.numericToTextFormat`: each 16-bit group as minimal
///     lowercase hex, joined by `:`, with NO `::` zero-compression), e.g.
///     `::1` → `0:0:0:0:0:0:0:1`, `fe80::1` → `fe80:0:0:0:0:0:0:1`. Rust's
///     `Ipv6Addr::to_string()` would otherwise emit the RFC-5952 compressed
///     form (`::1`), diverging from HotSpot's `getHostAddress()`.
///   * Plain IPv4 and unparseable hosts pass through unchanged.
fn hotspot_ip_string(ip: &str) -> String {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => v6
                .segments()
                .iter()
                .map(|seg| format!("{seg:x}"))
                .collect::<Vec<_>>()
                .join(":"),
        },
        Ok(std::net::IpAddr::V4(v4)) => v4.to_string(),
        Err(_) => ip.to_string(),
    }
}

fn alloc_inet_address(ctx: &mut dyn NativeContext, host: &str, ip: &str) -> ObjectRef {
    // Canonicalise the address string to HotSpot's exact textual form (v4-mapped
    // fold + uncompressed IPv6) — see [`hotspot_ip_string`] — so the mirror is an
    // `Inet4Address` with a 4-byte `getAddress()` where appropriate and
    // `getHostAddress()` is byte-identical to HotSpot.
    let ip_norm = hotspot_ip_string(ip);
    let ip = ip_norm.as_str();
    // Allocate the *concrete* address class so `instanceof Inet4Address`
    // checks (e.g. Hazelcast's `DefaultAddressPicker`) and virtual dispatch
    // resolve correctly. A bare `InetAddress` is abstract in real-JDK.
    let class_name = match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => "java/net/Inet6Address",
        _ => "java/net/Inet4Address",
    };
    let ia = alloc_concurrent_synthetic(ctx, class_name, 2);
    // Record host/IP in the ObjectRef-keyed side table — source of truth for
    // the natives we override. Do NOT write bare Strings into instance slots
    // 0/1: those are the real-JDK `holder` reference fields, and a String
    // there poisons real-JDK InetAddress bytecode dispatch (bogus
    // `NoSuchMethodError java/lang/String.getHostName()`). See
    // `inet_addr_side_table()` for the full rationale.
    inet_addr_set(ia, host, ip);
    // Additionally populate a *real* `InetAddress$InetAddressHolder` so any
    // un-overridden real-JDK `InetAddress` / `Inet4Address` bytecode (the
    // `final` `getHostName()` accessor, `toString()`, …) reads a consistent
    // shape instead of dereferencing a null `holder`.
    populate_inet_holder(ctx, ia, host, ip);
    ia
}

fn alloc_inet_socket_address(ctx: &mut dyn NativeContext, host: &str, port: i32) -> ObjectRef {
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
    let holder =
        alloc_concurrent_synthetic(ctx, "java/net/InetSocketAddress$InetSocketAddressHolder", 3);
    let h = ctx.create_string(host);
    ctx.set_field(holder, 0, Value::Object(Some(h)));
    ctx.set_field(holder, 1, Value::Object(None));
    ctx.set_field(holder, 2, Value::Int(port));
    ctx.set_field(isa, ISA_HOST, Value::Object(Some(holder)));
    ctx.set_field(isa, ISA_PORT, Value::Int(port));
    isa
}

/// Like [`alloc_inet_socket_address`], but populates the holder's `addr`
/// field with a REAL resolved `InetAddress` (`ip`) instead of leaving it
/// null. Real JDK's `ServerSocket`/`HttpServer.getAddress()` always returns
/// a fully-resolved address reflecting what the socket is actually bound to
/// (HotSpot: `/127.0.0.1:PORT`, both `getHostString()` and `getAddress()`
/// populated) — an unresolved echo (`hostString` = the raw ctor string,
/// `getAddress()` = null) breaks any caller that reconnects using the
/// server's own reported address (e.g. `RestClient.builder(new
/// HttpHost(address.getHostString(), ...))`): `getHostString()` on an
/// unresolved address returns the original hostname, and a caller-side TLS
/// connect that re-resolves it can land somewhere else entirely.
fn alloc_inet_socket_address_resolved(
    ctx: &mut dyn NativeContext,
    host: &str,
    ip: &str,
    port: i32,
) -> ObjectRef {
    let isa = alloc_concurrent_synthetic(ctx, "java/net/InetSocketAddress", 2);
    let holder =
        alloc_concurrent_synthetic(ctx, "java/net/InetSocketAddress$InetSocketAddressHolder", 3);
    let h = ctx.create_string(host);
    let addr = alloc_inet_address(ctx, host, ip);
    ctx.set_field(holder, 0, Value::Object(Some(h)));
    ctx.set_field(holder, 1, Value::Object(Some(addr)));
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
    let mut iter = std::net::ToSocketAddrs::to_socket_addrs(&lookup.as_str())
        .map_err(|e| uhex(format!("{host}: {e}")))?;
    match iter.next() {
        Some(sa) => Ok(sa.ip()),
        None => Err(uhex(format!("{host}"))),
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
pub(crate) fn uri_raw_string(ctx: &dyn NativeContext, uri: ObjectRef) -> String {
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

/// `java.net.URI.equals(Object)` — component-wise comparison, NOT raw-string
/// equality. Real JDK (`java.net.URI.equals`, see `java.base/java/net/URI.java`)
/// compares `scheme` and (for a server-based authority) `host`
/// case-INsensitively; `fragment`, `path`, `query`, `userInfo`, `port`, and a
/// registry-based `authority` are compared case-sensitively/exactly. Raw
/// string equality (the previous implementation here and in
/// `phases_early.rs`) collapses this into one exact-match test, which is only
/// an approximation — it under-fires whenever two URIs differ solely by
/// scheme/host case. That over-strictness is exactly what broke
/// `UriComponentsTests::toUriWithIpv6HostAlreadyEncoded[WHAT_WG]`: the WHATWG
/// URL Standard mandates lowercase IPv6 hosts in its canonical serialization
/// (`WhatWgUrlParser$Ipv6Address.serialize`, `Integer.toHexString` is always
/// lowercase), so `UriComponentsBuilder.fromUriString(..., WHAT_WG)` legitimately
/// lowercases a mixed-case IPv6 host like `5ABC` to `5abc` — but real JDK's
/// `URI.equals` still considers that URI equal to one with the original mixed
/// case, because host comparison ignores case. Raw-string equality does not,
/// so it reported the (correctly-lowercased) actual URI as unequal to the
/// (intentionally mixed-case) expected URI in the test.
pub(crate) fn uri_equals(ctx: &dyn NativeContext, a: ObjectRef, b: ObjectRef) -> bool {
    if a == b {
        return true;
    }
    let ra = uri_raw_string(ctx, a);
    let rb = uri_raw_string(ctx, b);
    if ra == rb {
        return true; // fast path — identical raw text is trivially equal
    }
    let (a_scheme, a_auth, a_path, a_query, a_frag) = uri_split(&ra);
    let (b_scheme, b_auth, b_path, b_query, b_frag) = uri_split(&rb);

    if !opt_str_eq_ignore_case(&a_scheme, &b_scheme) {
        return false;
    }
    if a_frag != b_frag {
        return false;
    }

    let a_opaque = a_scheme.is_some() && a_auth.is_none() && !a_path.starts_with('/');
    let b_opaque = b_scheme.is_some() && b_auth.is_none() && !b_path.starts_with('/');
    if a_opaque != b_opaque {
        return false;
    }
    if a_opaque {
        // `a_path` holds the scheme-specific part in this case (see `uri_split`).
        return a_path == b_path;
    }

    if a_path != b_path || a_query != b_query {
        return false;
    }

    match (&a_auth, &b_auth) {
        (None, None) => true,
        (Some(aa), Some(ba)) => {
            let (a_user, a_host, a_port) = uri_parse_authority(aa);
            let (b_user, b_host, b_port) = uri_parse_authority(ba);
            match (&a_host, &b_host) {
                (Some(_), Some(_)) => {
                    // Server-based authority: userInfo/port exact, host
                    // case-insensitive (RFC 3986 §3.2.2 — host is
                    // case-insensitive; this is the fix for the WHATWG
                    // IPv6-casing case above).
                    a_user == b_user && opt_str_eq_ignore_case(&a_host, &b_host) && a_port == b_port
                }
                // Registry-based (or unparsable) authority: compare the raw
                // authority string exactly, matching JDK's fallback branch.
                _ => aa == ba,
            }
        }
        _ => false,
    }
}

/// `java.net.URI.hashCode()` companion to [`uri_equals`] — MUST agree with it
/// (equal objects must have equal hashes). Hashes scheme/host as lowercase so
/// two URIs that `uri_equals` considers equal (differing only in scheme/host
/// case) also hash the same, mirroring real JDK's `hashIgnoringCase`.
pub(crate) fn uri_hash_code(ctx: &dyn NativeContext, uri: ObjectRef) -> i32 {
    let raw = uri_raw_string(ctx, uri);
    let (scheme, auth, path, query, frag) = uri_split(&raw);
    let mut h: i32 = 0;
    h = hash_str_ignore_case(h, scheme.as_deref());
    h = hash_str(h, frag.as_deref());
    let opaque = scheme.is_some() && auth.is_none() && !path.starts_with('/');
    if opaque {
        h = hash_str(h, Some(&path));
        return h;
    }
    h = hash_str(h, Some(&path));
    h = hash_str(h, query.as_deref());
    match &auth {
        Some(a) => {
            let (user, host, port) = uri_parse_authority(a);
            if host.is_some() {
                h = hash_str(h, user.as_deref());
                h = hash_str_ignore_case(h, host.as_deref());
                h = h.wrapping_add(1949i32.wrapping_mul(port));
            } else {
                h = hash_str(h, Some(a));
            }
        }
        None => {}
    }
    h
}

fn cmp_order(o: std::cmp::Ordering) -> i32 {
    match o {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

fn cmp_i32(a: i32, b: i32) -> i32 {
    cmp_order(a.cmp(&b))
}

fn cmp_str(a: &str, b: &str) -> i32 {
    cmp_order(a.cmp(b))
}

fn cmp_opt_str(a: Option<&str>, b: Option<&str>) -> i32 {
    match (a, b) {
        (None, None) => 0,
        (None, Some(_)) => -1,
        (Some(_), None) => 1,
        (Some(x), Some(y)) => cmp_str(x, y),
    }
}

fn cmp_opt_str_ci(a: Option<&str>, b: Option<&str>) -> i32 {
    match (a, b) {
        (None, None) => 0,
        (None, Some(_)) => -1,
        (Some(_), None) => 1,
        (Some(x), Some(y)) => cmp_str(&x.to_ascii_lowercase(), &y.to_ascii_lowercase()),
    }
}

/// `java.net.URI.compareTo(URI)` companion to `uri_equals`.
///
/// The real JDK orders URIs by components rather than by object identity. A
/// placeholder native used to return zero for every pair, which made callers
/// such as Apache POI treat `/xl/_rels/workbook.xml.rels` as equal to
/// `/_rels/.rels` and serialize package relationships relative to `/`.
fn uri_compare(ctx: &dyn NativeContext, a: ObjectRef, b: ObjectRef) -> i32 {
    if a == b {
        return 0;
    }
    let ra = uri_raw_string(ctx, a);
    let rb = uri_raw_string(ctx, b);
    if ra == rb {
        return 0;
    }
    let (a_scheme, a_auth, a_path, a_query, a_frag) = uri_split(&ra);
    let (b_scheme, b_auth, b_path, b_query, b_frag) = uri_split(&rb);

    let c = cmp_opt_str_ci(a_scheme.as_deref(), b_scheme.as_deref());
    if c != 0 {
        return c;
    }

    let a_opaque = a_scheme.is_some() && a_auth.is_none() && !a_path.starts_with('/');
    let b_opaque = b_scheme.is_some() && b_auth.is_none() && !b_path.starts_with('/');
    if a_opaque != b_opaque {
        return if a_opaque { 1 } else { -1 };
    }

    if a_opaque {
        let c = cmp_str(&a_path, &b_path);
        if c != 0 {
            return c;
        }
        return cmp_opt_str(a_frag.as_deref(), b_frag.as_deref());
    }

    match (&a_auth, &b_auth) {
        (Some(aa), Some(ba)) => {
            let (a_user, a_host, a_port) = uri_parse_authority(aa);
            let (b_user, b_host, b_port) = uri_parse_authority(ba);
            if a_host.is_some() && b_host.is_some() {
                let c = cmp_opt_str(a_user.as_deref(), b_user.as_deref());
                if c != 0 {
                    return c;
                }
                let c = cmp_opt_str_ci(a_host.as_deref(), b_host.as_deref());
                if c != 0 {
                    return c;
                }
                let c = cmp_i32(a_port, b_port);
                if c != 0 {
                    return c;
                }
            } else {
                let c = cmp_str(aa, ba);
                if c != 0 {
                    return c;
                }
            }
        }
        (None, Some(_)) => return -1,
        (Some(_), None) => return 1,
        (None, None) => {}
    }

    let c = cmp_str(&a_path, &b_path);
    if c != 0 {
        return c;
    }
    let c = cmp_opt_str(a_query.as_deref(), b_query.as_deref());
    if c != 0 {
        return c;
    }
    cmp_opt_str(a_frag.as_deref(), b_frag.as_deref())
}

fn opt_str_eq_ignore_case(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => false,
    }
}

fn hash_str(h: i32, s: Option<&str>) -> i32 {
    match s {
        Some(s) => s
            .bytes()
            .fold(h, |acc, b| acc.wrapping_mul(31).wrapping_add(b as i32)),
        None => h,
    }
}

fn hash_str_ignore_case(h: i32, s: Option<&str>) -> i32 {
    match s {
        Some(s) => s.bytes().fold(h, |acc, b| {
            acc.wrapping_mul(31)
                .wrapping_add(b.to_ascii_lowercase() as i32)
        }),
        None => h,
    }
}

/// Percent-decode a URI component the way `java.net.URI` getters do: each
/// `%XX` triplet is one byte, the byte sequence is interpreted as UTF-8, and
/// every other character (INCLUDING `+`, which URI leaves literal — unlike
/// `application/x-www-form-urlencoded`) is copied verbatim. A malformed `%`
/// escape (missing/non-hex digits) is copied through unchanged.
fn uri_percent_decode(input: &str) -> String {
    if !input.contains('%') {
        return input.to_string();
    }
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Select the raw (still percent-encoded) path of a URI from its full text,
/// matching `java.net.URI` path semantics:
///   * Returns `None` for an OPAQUE URI — one that is absolute (has a scheme)
///     and whose scheme-specific-part does not begin with `/` (e.g.
///     `mailto:x@y.com`, `urn:isbn:0`, `news:comp.lang.java`). Such URIs have a
///     null path; `getPath()`/`getRawPath()` must return null, not the SSP.
///   * Returns `Some(path)` for a HIERARCHICAL URI, where `path` may be the
///     empty string (e.g. `http://h` — authority but no path — yields `""`,
///     NOT null).
/// The scheme delimiter is the first `:` that precedes any `/`, `?` or `#`;
/// otherwise the `:` sits inside a relative-reference path and there is no
/// scheme. The path ends at the first `?` or `#`.
pub(crate) fn uri_select_raw_path(raw: &str) -> Option<String> {
    let (is_absolute, ssp) = match raw.find(':') {
        Some(i) => {
            let scheme = &raw[..i];
            let scheme_ok = !scheme.is_empty()
                && !scheme.contains('/')
                && !scheme.contains('?')
                && !scheme.contains('#');
            if scheme_ok {
                (true, &raw[i + 1..])
            } else {
                (false, raw)
            }
        }
        None => (false, raw),
    };
    if is_absolute && !ssp.starts_with('/') {
        return None; // opaque URI → null path
    }
    // Hierarchical: strip an optional `//authority`, then the trailing
    // query/fragment. An authority with no following path yields `""`.
    let after_auth = if let Some(rest) = ssp.strip_prefix("//") {
        match rest.find(['/', '?', '#']) {
            Some(p) => &rest[p..],
            None => "",
        }
    } else {
        ssp
    };
    let end = after_auth.find(['?', '#']).unwrap_or(after_auth.len());
    Some(after_auth[..end].to_string())
}

/// Byte index of the scheme-terminating `:`, or `None` for a relative
/// reference. Mirrors the real JDK parser (`uri_scheme_name_fail_index` in
/// lib.rs): scan for the first stop char among `:/?#`; only a `:` counts,
/// and the text before it must be a valid scheme name (ALPHA start,
/// alphanum/`+`/`-`/`.` body). Without this rule a colon inside a relative
/// path ("/redirect:account", Spring's view-name redirect tests) was taken
/// as a scheme delimiter, corrupting scheme/ssp/path derivation.
pub(crate) fn uri_scheme_colon(raw: &str) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut p = 0usize;
    while p < bytes.len() {
        match bytes[p] {
            b'/' | b'?' | b'#' => return None,
            b':' => break,
            _ => p += 1,
        }
    }
    if p == 0 || p >= bytes.len() {
        return None;
    }
    if !bytes[0].is_ascii_alphabetic() {
        return None;
    }
    if !bytes[1..p]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        return None;
    }
    Some(p)
}

/// Raw scheme-specific part, excluding the fragment delimiter and fragment.
/// `java.net.URI` treats `#fragment` as outside the SSP for both opaque
/// (`mailto:a#b`) and hierarchical (`https://h/p#b`) URIs. For a relative
/// reference (no valid scheme) the SSP is the whole input minus fragment.
fn uri_raw_scheme_specific_part(raw: &str) -> String {
    let ssp = match uri_scheme_colon(raw) {
        Some(colon) => &raw[colon + 1..],
        None => raw,
    };
    let end = ssp.find('#').unwrap_or(ssp.len());
    ssp[..end].to_string()
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
    let trailing_slash = path.ends_with('/') || matches!(segs.last(), Some(&".") | Some(&".."));
    let mut out: Vec<&str> = Vec::new();
    for seg in &segs {
        match *seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
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
fn uri_split(
    s: &str,
) -> (
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
) {
    let (without_frag, fragment) = match s.find('#') {
        Some(i) => (&s[..i], Some(s[i + 1..].to_string())),
        None => (s, None),
    };
    // scheme: leading "alpha *( alpha / digit / + / - / . ) :"
    let (scheme, rest) = match without_frag.find(':') {
        Some(i)
            if i > 0
                && without_frag[..i]
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_alphabetic())
                    .unwrap_or(false)
                && without_frag[..i]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
        {
            (Some(without_frag[..i].to_string()), &without_frag[i + 1..])
        }
        _ => (None, without_frag),
    };

    // Opaque absolute URIs (`scheme:ssp`) do not have authority, path, or query
    // components. A literal '?' belongs to the SSP; only '#' starts a fragment.
    if scheme.is_some() && !rest.starts_with('/') {
        return (scheme, None, rest.to_string(), None, fragment);
    }

    let (without_query, query) = match rest.find('?') {
        Some(i) => (&rest[..i], Some(rest[i + 1..].to_string())),
        None => (rest, None),
    };
    let (authority, path) = if let Some(after) = without_query.strip_prefix("//") {
        let end = after.find('/').unwrap_or(after.len());
        (Some(after[..end].to_string()), after[end..].to_string())
    } else {
        (None, without_query.to_string())
    };
    (scheme, authority, path, query, fragment)
}

/// Split a URI authority (`[userinfo "@"] host [":" port]`, RFC 3986 §3.2) into
/// `(userInfo, host, port)`. `port` is -1 when absent or unparsable. Host of an
/// IPv6 literal keeps its brackets (`[::1]`), matching `java.net.URI.getHost()`.
pub(crate) fn uri_parse_authority(authority: &str) -> (Option<String>, Option<String>, i32) {
    // userinfo ends at the last '@' (host cannot contain '@').
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (Some(authority[..i].to_string()), &authority[i + 1..]),
        None => (None, authority),
    };
    let (host, port_str) = if hostport.starts_with('[') {
        // IPv6 literal: host is "[...]", optional ":port" after the ']'.
        match hostport.find(']') {
            Some(j) => {
                let h = hostport[..=j].to_string();
                let p = hostport[j + 1..].strip_prefix(':').map(|x| x.to_string());
                (h, p)
            }
            None => (hostport.to_string(), None),
        }
    } else {
        // Reg-name: port (if any) follows the last ':'.
        match hostport.rfind(':') {
            Some(j) => (
                hostport[..j].to_string(),
                Some(hostport[j + 1..].to_string()),
            ),
            None => (hostport.to_string(), None),
        }
    };
    let port = port_str
        .and_then(|p| {
            if p.is_empty() {
                None
            } else {
                p.parse::<i32>().ok()
            }
        })
        .unwrap_or(-1);
    let host = if host.is_empty() { None } else { Some(host) };
    (userinfo, host, port)
}

/// Match `java.net.URI`'s server-based host acceptance for the cases keycloak's
/// validators care about: a dotted-decimal that LOOKS like IPv4 must be a valid
/// IPv4 (4 octets, each 0-255) or the host is rejected (null). IPv6 literals
/// (`[...]`) and reg-names (anything not pure digits+dots) are accepted.
fn uri_host_is_valid(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    if host.starts_with('[') {
        return true; // IPv6 literal
    }
    let looks_ipv4 = host.contains('.')
        && host
            .split('.')
            .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()));
    if looks_ipv4 {
        let segs: Vec<&str> = host.split('.').collect();
        return segs.len() == 4
            && segs
                .iter()
                .all(|s| s.parse::<u32>().map(|n| n <= 255).unwrap_or(false));
    }
    true // reg-name
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
        if let Some(a) = &authority {
            s.push_str("//");
            s.push_str(a);
        }
        s.push_str(&path);
        if let Some(q) = &query {
            s.push('?');
            s.push_str(q);
        }
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
    ctx.set_field_by_name(
        uri_obj,
        "decodedSchemeSpecificPart",
        Value::Object(Some(dssp)),
    );
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
        // Parse from raw string — JDK scheme rules (a colon inside a relative
        // path is NOT a scheme delimiter, see `uri_scheme_colon`).
        let raw = uri_raw_string(ctx, this);
        match uri_scheme_colon(&raw) {
            Some(i) => Ok(Some(Value::Object(Some(ctx.create_string(&raw[..i]))))),
            None => Ok(Some(Value::Object(None))),
        }
    });

    // getSchemeSpecificPart() → everything after 'scheme:', percent-DECODED.
    // The JDK returns the decoded scheme-specific part here (the raw form is
    // `getRawSchemeSpecificPart()` below). Skipping the decode left `%20` (and
    // other escapes) intact, so `new JarFile(uri.getSchemeSpecificPart())` in
    // Hibernate's `JarFileBasedArchiveDescriptor` opened a non-existent
    // `space%20par.par` instead of `space par.par` (PackagedEntityManagerTest
    // testSpacePar: "Unable to locate persistence.xml"). Decode mirrors the
    // already-correct `getPath()`. A path without escapes decodes to itself, so
    // the Spring Boot launcher path (no `%`) is unaffected.
    r.register(
        uri,
        "getSchemeSpecificPart",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let raw = uri_raw_string(ctx, this);
            let ssp = uri_raw_scheme_specific_part(&raw);
            let decoded = uri_percent_decode(&ssp);
            Ok(Some(Value::Object(Some(ctx.create_string(&decoded)))))
        },
    );

    // getRawSchemeSpecificPart() → same (we don't encode/decode)
    r.register(
        uri,
        "getRawSchemeSpecificPart",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let raw = uri_raw_string(ctx, this);
            let ssp = uri_raw_scheme_specific_part(&raw);
            Ok(Some(Value::Object(Some(ctx.create_string(&ssp)))))
        },
    );

    // getPath() → path field (by name) if set, else parse from raw. Unlike
    // getRawPath(), `getPath` returns the DECODED path: java.net.URI stores the
    // raw (percent-encoded) path in the `path` field (and make_uri likewise
    // stores the raw split component), so we must percent-decode before
    // returning. Without this, a URI like `otpauth://totp/Test%20Realm:tester`
    // yielded `/Test%20Realm:tester` from getPath() where the JDK returns the
    // decoded `/Test Realm:tester` (keycloak OtpPolicyTest label assertions).
    r.register(uri, "getPath", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        // Opaque URIs (e.g. `mailto:x@y.com`) have a null path. Decide from the
        // raw text rather than the `path` field, because `make_uri` stores a
        // `path` field for opaque URIs too (from `uri_split`), which would
        // otherwise surface the scheme-specific-part as the path.
        let parsed = match uri_select_raw_path(&raw) {
            None => return Ok(Some(Value::Object(None))),
            Some(p) => p,
        };
        // Hierarchical: prefer the real-JDK `path` field (raw, slot-order safe);
        // fall back to the parsed path (which may legitimately be "" for an
        // authority-only URI like `http://h`). Then percent-decode — getPath()
        // returns the DECODED path (getRawPath() below returns it raw).
        //
        // Slot-collision guard: synthetic URIs (URL.toURI's 7-slot layout,
        // raw string at slot 6) answer the by-name "path" read with the REAL
        // class's field index — which lands on the raw-string slot. The
        // symptom is the by-name value equalling the ENTIRE raw URI
        // ("file:/C:/...") — a real hierarchical path can never contain the
        // scheme prefix. Fall back to the parsed path in that case (Gradle's
        // new File(url.toURI()) yielded "file:\C:\..." Files, emptying every
        // ProjectBuilder module classpath).
        let mut raw_path = parsed;
        if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "path") {
            if let Some(v) = ctx.read_string(s) {
                let relative_without_authority =
                    !raw.starts_with('/') && !raw.starts_with("//") && raw.find(':').is_none();
                if !v.is_empty() && v != raw && !(relative_without_authority && v != raw_path) {
                    raw_path = v;
                }
            }
        }
        let decoded = uri_percent_decode(&raw_path);
        Ok(Some(Value::Object(Some(ctx.create_string(&decoded)))))
    });

    // getRawPath() → same path selection as getPath() but WITHOUT decoding.
    // Opaque URIs → null; hierarchical authority-only → "".
    r.register(uri, "getRawPath", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        let parsed = match uri_select_raw_path(&raw) {
            None => return Ok(Some(Value::Object(None))),
            Some(p) => p,
        };
        // Same slot-collision guard as getPath() above.
        let mut raw_path = parsed;
        if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "path") {
            if let Some(v) = ctx.read_string(s) {
                let relative_without_authority =
                    !raw.starts_with('/') && !raw.starts_with("//") && raw.find(':').is_none();
                if !v.is_empty() && v != raw && !(relative_without_authority && v != raw_path) {
                    raw_path = v;
                }
            }
        }
        Ok(Some(Value::Object(Some(ctx.create_string(&raw_path)))))
    });

    // getHost() → host field (1) or parsed from the raw authority.
    r.register(uri, "getHost", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Parse the host out of the raw authority FIRST — `native_uri_init`
        // populates the synthetic host slot inconsistently (empty for
        // "http://proxy1:8080", but the whole "my-example.com?auth.this" for a
        // URL with a query), so the raw string is the authoritative source.
        // (keycloak ProxyMappings host=null, and HostnameV2/ResourceIndicator
        // URL validation where getHost wrongly included the query.)
        let raw = uri_raw_string(ctx, this);
        let (_, auth_opt, _, _, _) = uri_split(&raw);
        if let Some(auth) = auth_opt {
            // The raw string HAS an authority section — it is authoritative, even
            // when empty (`file:///p`, `http://?q` → null host) or when the host
            // is a malformed IPv4 literal. java.net.URI's server-based parser
            // returns null for those; matching it makes keycloak's URL validators
            // reject them (HostnameV2 `192.196.0.5555`, `?my-example.com`).
            let (_, host_opt, _) = uri_parse_authority(&auth);
            let host = match host_opt {
                Some(h) if uri_host_is_valid(&h) => Value::Object(Some(ctx.create_string(&h))),
                _ => Value::Object(None),
            };
            return Ok(Some(host));
        }
        // No authority section in the raw string → fall back to an explicit host
        // slot set during construction.
        if let Value::Object(Some(s)) = ctx.get_field(this, 1) {
            if let Some(v) = ctx.read_string(s) {
                if !v.is_empty() {
                    return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
                }
            }
        }
        Ok(Some(Value::Object(None)))
    });

    // getPort() → port field (2) when a real port was stored, else parse the raw
    // authority. Absent port is -1 (java.net.URI contract), not the int-default 0.
    r.register(uri, "getPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Int(p) = ctx.get_field(this, 2) {
            if p > 0 {
                return Ok(Some(Value::Int(p)));
            }
        }
        let raw = uri_raw_string(ctx, this);
        if let (_, Some(auth), _, _, _) = uri_split(&raw) {
            let (_, _, port) = uri_parse_authority(&auth);
            return Ok(Some(Value::Int(port)));
        }
        Ok(Some(Value::Int(-1)))
    });

    // getUserInfo() → decoded user-information from the raw authority. Was
    // unregistered (real bytecode read an unpopulated field → null), so
    // `URI.create("http://user:pass@host:88").getUserInfo()` returned null
    // (keycloak ProxyMappings proxy-authentication case).
    r.register(uri, "getUserInfo", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        if let (_, Some(auth), _, _, _) = uri_split(&raw) {
            if let (Some(ui), _, _) = uri_parse_authority(&auth) {
                let decoded = uri_percent_decode(&ui);
                return Ok(Some(Value::Object(Some(ctx.create_string(&decoded)))));
            }
        }
        Ok(Some(Value::Object(None)))
    });

    // getQuery() → decoded hierarchical query. Opaque URIs keep a literal '?'
    // inside the scheme-specific part, so parse from `uri_split` rather than a
    // raw `find('?')` fallback.
    r.register(uri, "getQuery", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        if !raw.is_empty() {
            let (_, _, _, query, _) = uri_split(&raw);
            return match query {
                Some(q) => {
                    let decoded = uri_percent_decode(&q);
                    Ok(Some(Value::Object(Some(ctx.create_string(&decoded)))))
                }
                None => Ok(Some(Value::Object(None))),
            };
        }
        // Last-ditch field fallback for any pre-populated real-JDK URI object
        // that has a query field but no raw-string cache visible to CratonVM.
        if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "query") {
            if let Some(v) = ctx.read_string(s) {
                let decoded = uri_percent_decode(&v);
                return Ok(Some(Value::Object(Some(ctx.create_string(&decoded)))));
            }
        }
        Ok(Some(Value::Object(None)))
    });

    // getFragment() → the (decoded) fragment after '#', parsed from the raw
    // string. The previous `fragment` field-by-name read was unreliable: for a
    // `new URI(string)` (native_uri_init) URL it collided with the host/SSP slot
    // and returned e.g. "something" as the fragment of "https://something"
    // (keycloak ResourceIndicator/HostnameV2 URL validation: getFragment()!=null
    // wrongly rejected valid URLs). Parsing the raw string is authoritative for
    // both synthetic and real-JDK-constructed URIs and also yields the correct
    // null for a missing fragment (no spurious "#null").
    r.register(uri, "getFragment", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        match raw.find('#') {
            Some(i) => {
                let decoded = uri_percent_decode(&raw[i + 1..]);
                Ok(Some(Value::Object(Some(ctx.create_string(&decoded)))))
            }
            None => Ok(Some(Value::Object(None))),
        }
    });

    // getRawFragment() → raw (undecoded) fragment after '#', else null.
    r.register(
        uri,
        "getRawFragment",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let raw = uri_raw_string(ctx, this);
            match raw.find('#') {
                Some(i) => Ok(Some(Value::Object(Some(ctx.create_string(&raw[i + 1..]))))),
                None => Ok(Some(Value::Object(None))),
            }
        },
    );

    // getRawQuery() → raw hierarchical query, else null. For opaque URIs,
    // '?' is part of the raw scheme-specific part and must not surface here.
    r.register(uri, "getRawQuery", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        if !raw.is_empty() {
            let (_, _, _, query, _) = uri_split(&raw);
            return match query {
                Some(q) => Ok(Some(Value::Object(Some(ctx.create_string(&q))))),
                None => Ok(Some(Value::Object(None))),
            };
        }
        match ctx.get_field_by_name(this, "query") {
            Value::Object(Some(s)) => Ok(Some(Value::Object(Some(s)))),
            _ => Ok(Some(Value::Object(None))),
        }
    });

    // getRawUserInfo() → raw (undecoded) userinfo from the authority, else null.
    r.register(
        uri,
        "getRawUserInfo",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let raw = uri_raw_string(ctx, this);
            if let (_, Some(auth), _, _, _) = uri_split(&raw) {
                if let (Some(ui), _, _) = uri_parse_authority(&auth) {
                    return Ok(Some(Value::Object(Some(ctx.create_string(&ui)))));
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );

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
            return Err(iae("URI is not absolute"));
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
        // `war` is Tomcat's scheme, registered via
        // `TomcatURLStreamHandlerFactory.register()` →
        // `URL.setURLStreamHandlerFactory`. We don't model the factory
        // registry, but `war:` is unambiguous (not a Windows drive letter like
        // the `C:` case the unknown-protocol path deliberately rejects), so
        // accepting it lets `URI.create("war:file:/...").toURL()` succeed —
        // which `UriUtil.warToJar` (and WebResources) rely on.
        const KNOWN_PROTOCOLS: &[&str] = &[
            "file", "jar", "http", "https", "ftp", "jrt", "jmod", "mailto", "news", "jndi", "war",
        ];
        let proto_lc = proto.to_ascii_lowercase();
        if proto.is_empty() {
            // No scheme: the real `URI.toURL()` throws IllegalArgumentException
            // ("URI is not absolute"); Tomcat's catch handles that too.
            return Err(iae("URI is not absolute"));
        }
        // Hierarchical (authority-bearing) schemes that have a real built-in
        // JDK URL stream handler MUST be parsed by the real `java.net.URL`
        // constructor too: the synthetic build below never splits the
        // `//host:port` authority out of the path, so it leaves host="" /
        // port=-1 and stuffs `//host:port/path` into the file field. That
        // silently broke any caller that inspects URL host/port/file — e.g.
        // `Response.isEncodeable` (URL session-id rewriting) returned false for
        // every same-origin URL, so `encodeURL`/`encodeRedirectURL` never
        // appended `;jsessionid=…` (TestResponse: 30 failures). `new
        // URL(String)` is un-intercepted real bytecode in real-JDK mode and
        // parses the authority correctly, so defer these to it.
        const AUTHORITY_HANDLER_SCHEMES: &[&str] = &["http", "https", "ftp"];
        if !KNOWN_PROTOCOLS.contains(&proto_lc.as_str())
            || AUTHORITY_HANDLER_SCHEMES.contains(&proto_lc.as_str())
        {
            // Not one of the synthetic-only built-in schemes. Rather than
            // blindly reject (unknown scheme) or mangle the authority
            // (http/https/ftp), defer to the REAL `java.net.URL` constructor,
            // which runs `URL.getURLStreamHandler(proto)` — consulting any
            // app-registered `URLStreamHandlerFactory` (published into
            // `URL.factory` by `native_url_set_stream_handler_factory_guard`).
            // This is what lets Tomcat's `classpath:` scheme resolve via its
            // `TomcatURLStreamHandlerFactory` (`URI.create("classpath:…").toURL()`
            // in TestConfigFileLoader / TestClasspathUrlStreamHandler) while
            // STILL throwing `MalformedURLException` for genuinely-unknown
            // schemes — including the single-letter Windows drive (`C:`) case
            // that Tomcat's `Bootstrap.createClassLoader` relies on catching to
            // fall into its `*.jar` glob-expansion branch (the real `URL("C:/…")`
            // ctor throws `unknown protocol: c` exactly as the old hard-coded
            // reject did). `new URL(String)` is un-intercepted real bytecode in
            // real-JDK mode, so this honours the full real handler-lookup path.
            let full_s = ctx.create_string(&raw);
            return ctx.new_object_initialized(
                "java/net/URL",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(full_s))],
            );
        }
        // Authority-based hierarchical URI (`scheme://host[:port]/path`):
        // delegate to the real `java.net.URL(String)` constructor, which
        // correctly splits host/port/file. The hand-rolled synthetic build
        // below stuffs the WHOLE `//host:port/path` scheme-specific-part into
        // the `file` slot and leaves `host` empty (slot 1 = ""), so the
        // resulting URL's getHost()/getPort()/getAuthority() disagree with
        // `new URL(spec)` — and URL.equals (which compares protocol+host+port)
        // then returns false for two URLs whose toString() is identical.
        // Spring's URLEditor/UrlResource build URLs via URI.toURL(), and
        // UrlSet.setUrlNames calls URI.toURL() directly, so the broken host
        // split made BeanFactoryGenericsTests' NamedUrlList/Set/Map element
        // conversion (and setBean) produce URLs that compare unequal to the
        // expected `new URL(...)`. Opaque / non-authority schemes (file:/C:/…,
        // jar:file:…!/…) whose SSP has no `//` keep the hand-rolled path, where
        // `file == scheme-specific-part` is the intended shape that Tomcat and
        // Gradle file:/jar: handling rely on.
        if raw[proto.len() + 1..].starts_with("//") {
            let full_s = ctx.create_string(&raw);
            return ctx.new_object_initialized(
                "java/net/URL",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(full_s))],
            );
        }
        // Build a simple 13-field synthetic URL (same layout as p59_alloc_url).
        let url = alloc_concurrent_synthetic(ctx, "java/net/URL", 13);
        let file = if proto.is_empty() {
            &raw[..]
        } else {
            &raw[proto.len() + 1..]
        };
        let (file_without_ref, ref_part) = if let Some(pos) = file.find('#') {
            (&file[..pos], Some(&file[pos + 1..]))
        } else {
            (file, None)
        };
        let (path, query_part) = if let Some(pos) = file_without_ref.find('?') {
            (&file_without_ref[..pos], Some(&file_without_ref[pos + 1..]))
        } else {
            (file_without_ref, None)
        };
        let proto_s = ctx.create_string(proto);
        let file_s = ctx.create_string(file_without_ref);
        let path_s = ctx.create_string(path);
        let query_s = query_part
            .filter(|query| !query.is_empty())
            .map(|query| ctx.create_string(query));
        let ref_s = ref_part
            .filter(|fragment| !fragment.is_empty())
            .map(|fragment| ctx.create_string(fragment));
        let host_s = ctx.create_string("");
        ctx.set_field(url, 0, Value::Object(Some(proto_s)));
        ctx.set_field(url, 1, Value::Object(Some(host_s)));
        ctx.set_field(url, 2, Value::Int(-1));
        ctx.set_field(url, 3, Value::Object(Some(file_s)));
        ctx.set_field(url, 4, Value::Object(query_s));
        // Field 5 is the real `java.net.URL.authority` field — leave it
        // `null` (as real JDK does for a host-less URL) instead of stuffing
        // the whole raw URL string there. That anti-pattern (already fixed
        // once for `jboss_module_loader::build_synthetic_url` — see its doc
        // comment) makes real bytecode's `getAuthority()` return the entire
        // URL string, which contains '/'. When this URL is later used as
        // the *base* in `new URL(URL base, String spec)` (e.g. Woodstox's
        // `URLUtil.urlFromSystemId(String, URL)`, called while resolving a
        // DTD's external SYSTEM entity against the document's base URI),
        // `URLStreamHandler.parseURL`'s merge logic inherits that corrupted
        // authority into the merged URL and real JDK's own authority
        // validation rejects it with `MalformedURLException: Illegal
        // character found in authority: '/'` — even though the merge would
        // otherwise succeed (systemId is already absolute). See
        // Jaxb2CollectionHttpMessageConverterTests
        // .readXmlRootElementExternalEntityEnabled().
        ctx.set_field(url, 6, Value::Object(Some(path_s)));
        ctx.set_field(url, 8, Value::Object(ref_s));
        Ok(Some(Value::Object(Some(url))))
    });

    // compareTo(URI) -> component-wise ordering consistent with equals.
    r.register(uri, "compareTo", "(Ljava/net/URI;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Err(iae("URI.compareTo null")),
        };
        Ok(Some(Value::Int(uri_compare(ctx, this, other))))
    });

    // Bridge form used by erased Comparable call sites.
    r.register(uri, "compareTo", "(Ljava/lang/Object;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Err(iae("URI.compareTo null")),
        };
        Ok(Some(Value::Int(uri_compare(ctx, this, other))))
    });

    // equals(Object) — see `uri_equals` doc comment: real JDK compares
    // scheme/host case-insensitively, everything else case-sensitively; raw
    // string equality over-fires on exactly that mismatch (e.g. WHATWG IPv6
    // hosts, which the WHATWG URL Standard canonicalizes to lowercase).
    r.register(uri, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        Ok(Some(Value::Int(if uri_equals(ctx, this, other) {
            1
        } else {
            0
        })))
    });

    // hashCode() — must agree with `equals` (see `uri_hash_code`): hashing the
    // raw string breaks the equals/hashCode contract for URIs that differ
    // only by scheme/host case, since those compare equal.
    r.register(uri, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(uri_hash_code(ctx, this))))
    });

    // normalize() → RFC 3986 §5.2.4 remove-dot-segments on the PATH component
    // (matches `java.net.URI.normalize()`). The previous implementation returned
    // `this` unchanged, so a relative TLD reference like `../WEB-INF/test.tld`
    // (resolved by Jasper to `/jsp/../WEB-INF/test.tld`) was never collapsed to
    // `/WEB-INF/test.tld`. The un-normalized string then became the
    // `TldResourcePath.webappPath`, which did not equal the scanned TLD's
    // `/WEB-INF/test.tld` key, so the TldCache lookup missed and JSP compilation
    // 500'd ("Unable to find taglib ... for URI: [../WEB-INF/test.tld]").
    r.register(uri, "normalize", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        let (scheme, authority, path, query, fragment) = uri_split(&raw);
        // Opaque URIs (scheme present, no authority, path not starting with '/')
        // and empty paths have no hierarchical path to normalize — return as-is.
        let opaque = scheme.is_some() && authority.is_none() && !path.starts_with('/');
        if opaque || path.is_empty() {
            return Ok(Some(Value::Object(Some(this))));
        }
        let mut norm = uri_remove_dot_segments(&path);
        // For a relative path whose first segment ends up containing a ':',
        // prefix "./" so it cannot be re-parsed as a scheme (JDK does the same).
        if scheme.is_none() && authority.is_none() && !norm.starts_with('/') {
            if norm
                .split('/')
                .next()
                .map(|s| s.contains(':'))
                .unwrap_or(false)
            {
                norm = format!("./{norm}");
            }
        }
        if norm == path {
            return Ok(Some(Value::Object(Some(this))));
        }
        let recomposed = uri_recompose(&scheme, &authority, &norm, &query, &fragment);
        Ok(Some(Value::Object(Some(make_uri(ctx, &recomposed)))))
    });

    // resolve(URI) → RFC 3986 §5.2 reference resolution. The previous
    // implementation simply returned the argument, which is wrong for any
    // relative reference (e.g. ActiveMQ's `new URI(jarUrl).resolve("..")`
    // for locating ACTIVEMQ_HOME from the launcher jar). Without correct
    // resolution the launcher fell back to a literal `../.` home and could
    // not find lib/*.jar.
    r.register(
        uri,
        "resolve",
        "(Ljava/net/URI;)Ljava/net/URI;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let base = uri_raw_string(ctx, this);
            let reference = match args.get(1) {
                Some(Value::Object(Some(o))) => uri_raw_string(ctx, *o),
                _ => return Ok(Some(Value::Object(Some(this)))),
            };
            let resolved = uri_resolve_ref(&base, &reference);
            Ok(Some(Value::Object(Some(make_uri(ctx, &resolved)))))
        },
    );

    // resolve(String) → resolve(URI.create(str)). Registered explicitly so
    // the synthetic-URI path does not depend on JDK bytecode chaining.
    r.register(
        uri,
        "resolve",
        "(Ljava/lang/String;)Ljava/net/URI;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let base = uri_raw_string(ctx, this);
            let reference = match args.get(1) {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(Some(this)))),
            };
            let resolved = uri_resolve_ref(&base, &reference);
            Ok(Some(Value::Object(Some(make_uri(ctx, &resolved)))))
        },
    );

    // relativize(URI) -> JDK-compatible prefix relativization for hierarchical
    // URIs. Real bytecode reads URI internals that our synthetic constructors do
    // not always populate, so run this from the raw text instead.
    r.register(
        uri,
        "relativize",
        "(Ljava/net/URI;)Ljava/net/URI;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(Some(this)))),
            };
            let base = uri_raw_string(ctx, this);
            let target = uri_raw_string(ctx, other);
            let (b_scheme, b_auth, b_path, _b_query, _b_frag) = uri_split(&base);
            let (t_scheme, t_auth, t_path, t_query, t_frag) = uri_split(&target);
            let b_opaque = b_scheme.is_some() && b_auth.is_none() && !b_path.starts_with('/');
            let t_opaque = t_scheme.is_some() && t_auth.is_none() && !t_path.starts_with('/');
            if b_opaque
                || t_opaque
                || !opt_str_eq_ignore_case(&b_scheme, &t_scheme)
                || b_auth != t_auth
            {
                return Ok(Some(Value::Object(Some(other))));
            }
            let b_norm = uri_remove_dot_segments(&b_path);
            let t_norm = uri_remove_dot_segments(&t_path);
            if !t_norm.starts_with(&b_norm) {
                return Ok(Some(Value::Object(Some(other))));
            }
            let rel = &t_norm[b_norm.len()..];
            if rel.is_empty() {
                return Ok(Some(Value::Object(Some(make_uri(ctx, "")))));
            }
            if !b_norm.ends_with('/') && !rel.starts_with('/') {
                return Ok(Some(Value::Object(Some(other))));
            }
            let rel = rel.strip_prefix('/').unwrap_or(rel);
            let recomposed = uri_recompose(&None, &None, rel, &t_query, &t_frag);
            Ok(Some(Value::Object(Some(make_uri(ctx, &recomposed)))))
        },
    );

    // create(String) — static factory.
    //
    // Must produce a fully-parsed URI with its `scheme`/`path`/`authority`/…
    // fields populated BY NAME, exactly like every other URI constructor.
    // The previous body wrote the *whole* raw string into instance-field
    // slot index 6 ("raw-string cache" in the legacy synthetic 7-slot URI
    // layout). In real-JDK mode `java.net.URI`'s actual field 6 is `path`,
    // so `URI.getPath()` read back the full `file:/C:/…!/entry` string
    // instead of just `/C:/…!/entry`. SmallRye's
    // `AbstractLocationConfigSourceLoader.addProfileName` then re-wrapped
    // that already-`file:`-prefixed value, yielding the malformed
    // `jar:file:file:/…!/application-<profile>.properties` URL that aborts
    // Keycloak/Quarkus boot with `SRCFG00035`. `make_uri` parses the string
    // and sets all components by name (slot-order safe).
    r.register(
        uri,
        "create",
        "(Ljava/lang/String;)Ljava/net/URI;",
        |ctx, args| {
            let s_obj = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let s = ctx.read_string(s_obj).unwrap_or_default();
            // URI.create(String) translates URI(String) parse failures to
            // IllegalArgumentException, but valid results must keep make_uri's
            // field layout for the URI accessors used by Keycloak.
            if let Some((pos, reason)) = crate::uri_scheme_name_fail_index(&s) {
                return Err(iae(format!("{reason} at index {pos}: {s}")));
            }
            let strict_uri_chars = std::env::var("CRATONVM_URI_STRICT_CHARS")
                .map(|v| v != "0")
                .unwrap_or(true);
            let illegal = if strict_uri_chars {
                crate::uri_first_illegal_index(&s)
            } else {
                s.char_indices()
                    .find(|(_, c)| (*c as u32) < 0x20 || (*c as u32) == 0x7f)
                    .map(|(i, _)| i)
            };
            if let Some(pos) = illegal {
                return Err(iae(format!("Illegal character in URI at index {pos}: {s}")));
            }
            if let Some(pos) = crate::uri_empty_ssp_fail_index(&s) {
                return Err(iae(format!(
                    "Expected scheme-specific part at index {pos}: {s}"
                )));
            }
            Ok(Some(Value::Object(Some(make_uri(ctx, &s)))))
        },
    );
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
    let stream_id = sock_get(ctx, this).stream_id;
    if stream_id < 0 {
        return Err(ioex("Socket not connected"));
    }
    // Clone the cheap Arc<TcpStream> out under a SHORT lock, then release
    // s2_registry BEFORE the blocking read(). Holding the global registry lock
    // across a blocking read deadlocks every other synthetic-socket operation
    // process-wide: any peer thread that must WRITE the response (okhttp
    // MockWebServer's dispatcher, a request/response loopback, …) blocks
    // forever on the same lock while this read waits for bytes that only that
    // write can produce → the HTTP exchange times out (BUG-04 loopback hang).
    // The Arc shares the SAME socket fd (NOT a try_clone() duplicate — a recv
    // blocked on a Windows duplicate handle is not woken by data arriving after
    // it blocked), so reading via `&TcpStream` here is woken correctly while the
    // lock is free for the peer writer.
    let stream = {
        let reg = s2_registry().lock();
        reg.streams
            .get(&stream_id)
            .ok_or_else(|| ioex("Socket stream not found"))?
            .clone()
    };
    let dbg = std::env::var_os("CRATONVM_DBG_SOCK").is_some();
    if dbg {
        eprintln!("[dbg-sock] read: sid={stream_id} want={ln} (blocking on recv...)");
    }
    let mut tmp = vec![0u8; ln];
    let mut blocked_refs = [Value::Object(Some(buf))];
    ctx.begin_blocking_region();
    let read_result = loop {
        match (&*stream).read(&mut tmp) {
            Err(e)
                if e.kind() == std::io::ErrorKind::Interrupted || e.raw_os_error() == Some(4) =>
            {
                continue
            }
            result => break result,
        }
    };
    ctx.end_blocking_region_refs(&mut blocked_refs);

    let buf = match blocked_refs[0] {
        Value::Object(Some(o)) => o,
        _ => buf,
    };
    let n = read_result.map_err(|e| match e.kind() {
        // SO_RCVTIMEO is reported as TimedOut on Windows and often as
        // WouldBlock on Unix. Both are Java SocketTimeoutException, not EOF
        // and not a generic IOException; callers deliberately catch this
        // concrete type to retry their protocol operation.
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            RuntimeError::SocketTimeoutException {
                message: format!("Socket read timed out: {e}"),
            }
            .into()
        }
        _ => ioex(format!("Socket read failed: {e}")),
    })?;
    if dbg {
        eprintln!("[dbg-sock] read: sid={stream_id} got={n}");
        if std::env::var_os("CRATONVM_DBG_SOCK_BYTES").is_some() && n != 0 {
            eprintln!(
                "[dbg-sock-bytes] read: sid={stream_id} data={}",
                socket_dbg_bytes(&tmp[..n])
            );
        }
    }
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
    let stream_id = sock_get(ctx, this).stream_id;
    if stream_id < 0 {
        return Err(ioex("Socket not connected"));
    }
    // Clone the Arc out under a short lock and write on the shared `&TcpStream`
    // with the lock released — same-fd handle, so the bytes hit the wire
    // immediately (no deferred-until-close like a dropped try_clone duplicate)
    // and a concurrently-reading peer is never wedged behind the registry lock.
    let stream = {
        let reg = s2_registry().lock();
        reg.streams
            .get(&stream_id)
            .ok_or_else(|| ioex("Socket stream not found"))?
            .clone()
    };
    ctx.begin_blocking_region();
    let write_result = (|| -> std::io::Result<()> {
        (&*stream).write_all(&data)?;
        (&*stream).flush()?;
        Ok(())
    })();
    ctx.end_blocking_region();
    if let Err(e) = write_result {
        // Real java.net.Socket write path: a peer-reset/broken-pipe write
        // failure must surface as a real, catchable java.net.SocketException
        // (matching real JDK's SocketOutputStream.socketWrite0) -- callers
        // such as TestSwallowAbortedUploads doTestChunkedPUT() specifically catch
        // SocketException, and a bare java.io.IOException falls
        // through uncaught. Mirrors the SocketException classification
        // native-io/src/socket_channel.rs::map_err already does for the NIO
        // SocketChannel write path.
        let is_reset = matches!(
            e.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::NotConnected
        );
        if is_reset {
            let jmsg = ctx.create_string(&format!("Connection reset: {e}"));
            return match ctx.new_object_initialized(
                "java/net/SocketException",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(jmsg))],
            ) {
                Ok(Some(Value::Object(Some(exc)))) => {
                    let exc_pin = ctx.pin_native_root(exc);
                    let exc = ctx.read_native_pin(exc_pin, exc);
                    Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                        exc,
                    ))
                }
                _ => Err(ioex(format!("Socket write failed: {e}"))),
            };
        }
        return Err(ioex(format!("Socket write failed: {e}")));
    }
    if std::env::var_os("CRATONVM_DBG_SOCK").is_some() {
        eprintln!(
            "[dbg-sock] write: sid={stream_id} sent={} bytes",
            data.len()
        );
        if std::env::var_os("CRATONVM_DBG_SOCK_BYTES").is_some() {
            eprintln!(
                "[dbg-sock-bytes] write: sid={stream_id} data={}",
                socket_dbg_bytes(&data)
            );
        }
    }
    Ok(None)
}

fn native_socket_input_stream_read_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let owner =
        stream_owner_get(ctx, this).ok_or_else(|| ioex("SocketInputStream has no owner"))?;
    let buf = obj_arg(args, 1)?;
    let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
    re1_socket_read_stream(ctx, owner, buf, off, len)
}

fn native_socket_input_stream_read_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let owner =
        stream_owner_get(ctx, this).ok_or_else(|| ioex("SocketInputStream has no owner"))?;
    let buf = obj_arg(args, 1)?;
    let len = ctx.array_length(buf) as i32;
    re1_socket_read_stream(ctx, owner, buf, 0, len)
}

fn native_socket_input_stream_read_one(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let owner =
        stream_owner_get(ctx, this).ok_or_else(|| ioex("SocketInputStream has no owner"))?;
    let one = ctx.new_array(ArrayElementType::Byte, 1);
    let one_pin = ctx.pin_native_root(one);
    let r = re1_socket_read_stream(ctx, owner, one, 0, 1);
    let one = ctx.read_native_pin(one_pin, one);
    ctx.unpin_native_roots(one_pin);
    let r = r?;
    match r {
        Some(Value::Int(-1)) => Ok(Some(Value::Int(-1))),
        Some(Value::Int(_)) => {
            let b = ctx.get_array_element(one, 0).as_int().unwrap_or(0);
            Ok(Some(Value::Int(b & 0xff)))
        }
        _ => Ok(Some(Value::Int(-1))),
    }
}

/// The real `java.net.Socket` constructor initializes `socketLock = new Object()`
/// via a field initializer; the synthetic `<init>` natives skip it, so
/// `socketLock` reads back null. Real `Socket.getImpl()` bytecode — reached by
/// the option *getters* we do not override (`getReceiveBufferSize`,
/// `getKeepAlive`, …) and by `jdk.net.Sockets.<clinit>` (option-set probing) —
/// does `synchronized (socketLock)` → `NullPointerException: monitorenter in
/// java/net/Socket.getImpl pc=16`. Initialize the lock object(s) so that path
/// works. `set_field_by_name` is a no-op if the field is absent (synthetic-stub
/// mode), so this is safe on either layout.
/// Returns the (possibly GC-relocated) `this` — callers that keep using the
/// Socket afterward MUST use the returned value, not their original local.
#[must_use]
pub(crate) fn re1_init_socket_locks(ctx: &mut dyn NativeContext, this: ObjectRef) -> ObjectRef {
    // GC-safety: `this` is a raw ObjectRef parameter, and `new_object` below
    // is a re-entrant, allocating call (it can trigger a GC). This is called
    // right after a fresh Socket allocation -- for a freshly-accepted Socket
    // (see the `ss, "accept"` wrapper), the caller passes this same `this`
    // straight into `re2_accept_into` afterward, so a relocation/reclaim here
    // corrupts the Socket before it is ever returned to Java bytecode.
    // Pin across both allocating iterations, re-reading the current
    // (possibly-forwarded) value before every use.
    let pin_base = ctx.pin_native_root(this);
    for f in ["socketLock", "closeLock"] {
        let cur = ctx.read_native_pin(pin_base, this);
        if !matches!(ctx.get_field_by_name(cur, f), Value::Object(Some(_))) {
            if let Ok(Some(Value::Object(Some(lock)))) = ctx.new_object("java/lang/Object") {
                let cur = ctx.read_native_pin(pin_base, this);
                ctx.set_field_by_name(cur, f, Value::Object(Some(lock)));
            }
        }
    }
    let result = ctx.read_native_pin(pin_base, this);
    ctx.unpin_native_roots(pin_base);
    result
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
    // BUGFIX [nb-net-phase-e]: throw the CONCRETE `java.net.*` exception types
    // for connect failures, not a generic `IOException` whose message merely
    // mentions the class name as a text prefix — real code catches these by
    // type (`catch (ConnectException e)` / `catch (SocketTimeoutException e)`;
    // a bare IOException escapes both).
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::ConnectionRefused => RuntimeError::ConnectException {
            message: format!("{host}:{port}: {e}"),
        }
        .into(),
        std::io::ErrorKind::TimedOut => RuntimeError::SocketTimeoutException {
            message: format!("{host}:{port}: {e}"),
        }
        .into(),
        _ => ioex(format!("ConnectException: {host}:{port}: {e}")),
    })?;
    let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
    // `Socket.setSoTimeout` may have been called while this Socket was still
    // unconnected (Spring Boot's TcpConnectServiceReadinessCheck does exactly
    // that). The side table is the authoritative socket state for this native
    // surface, so apply its retained setting before publishing the stream.
    let read_timeout_ms = sock_get(ctx, this).read_timeout_ms;
    if read_timeout_ms > 0 {
        stream
            .set_read_timeout(Some(Duration::from_millis(read_timeout_ms as u64)))
            .map_err(|e| ioex(format!("apply preconnect SO_TIMEOUT failed: {e}")))?;
    }
    let stream_id = s2_alloc_stream(stream);
    let pin_base = ctx.pin_native_root(this);
    let host_str = ctx.create_string(host);
    let this_now = ctx.read_native_pin(pin_base, this);
    ctx.set_field(this_now, SOCK_HOST, Value::Object(Some(host_str)));
    sock_set(ctx, this_now, |s| {
        s.port = port;
        s.local_port = local_port;
        s.closed = 0;
        s.stream_id = stream_id;
    });
    let _ = re1_init_socket_locks(ctx, this_now);
    ctx.unpin_native_roots(pin_base);
    Ok(None)
}

fn re1_socket_adaptor_inet(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    local: bool,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let sc = match ctx.get_field_by_name(this, "sc") {
        Value::Object(Some(sc)) => sc,
        _ => return Ok(None),
    };
    let method = if local {
        "getLocalAddress"
    } else {
        "getRemoteAddress"
    };
    let socket_addr = match ctx.invoke_virtual(sc, method, "()Ljava/net/SocketAddress;", &[])? {
        Some(Value::Object(Some(addr))) => addr,
        _ => return Ok(None),
    };

    if let Ok(Some(Value::Object(Some(addr)))) =
        ctx.invoke_virtual(socket_addr, "getAddress", "()Ljava/net/InetAddress;", &[])
    {
        return Ok(Some(addr));
    }

    let host = match ctx.invoke_virtual(socket_addr, "getHostString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let host = if host.is_empty() {
        match ctx.invoke_virtual(socket_addr, "getHostName", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        }
    } else {
        host
    };
    if host.is_empty() {
        return Ok(None);
    }
    let ip = host.trim_matches(&['[', ']'][..]);
    Ok(Some(alloc_inet_address(ctx, ip, ip)))
}

fn register_re1_socket(r: &mut NativeMethodRegistry) {
    // NIO-SERVER-SOCKET (route 1): skip the synthetic java.net.Socket surface so
    // real bytecode drives sun/nio/ch/Net. See register_phase53_socket_stubs.
    if std::env::var_os("CRATONVM_REAL_NET_SOCKETS").is_some() {
        return;
    }
    let sock = "java/net/Socket";

    r.register(sock, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, SOCK_HOST, Value::Object(None));
        sock_set(ctx, this, |s| {
            s.port = 0;
            s.local_port = 0;
            s.closed = 0;
            s.stream_id = -1;
        });
        let _ = re1_init_socket_locks(ctx, this);
        Ok(None)
    });

    r.register(sock, "<init>", "(Ljava/lang/String;I)V", |ctx, args| {
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
    });

    r.register(sock, "<init>", "(Ljava/net/InetAddress;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = obj_arg(args, 1)?;
        let host = inet_addr_field_string_or(ctx, addr, IA_ADDR, "127.0.0.1");
        let port = args
            .get(2)
            .and_then(|v| v.as_int())
            .ok_or_else(|| iae("Socket: missing port"))?;
        re1_connect_socket(ctx, this, &host, port, 0)
    });

    r.register(
        sock,
        "connect",
        "(Ljava/net/SocketAddress;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sa = obj_arg(args, 1).map_err(|_| ioex("Socket.connect: null address"))?;
            let (host, port) = read_inet_socket_address(ctx, sa)?;
            re1_connect_socket(ctx, this, &host, port, 0)
        },
    );

    r.register(
        sock,
        "connect",
        "(Ljava/net/SocketAddress;I)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sa = obj_arg(args, 1).map_err(|_| ioex("Socket.connect: null address"))?;
            let timeout_ms = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            if timeout_ms < 0 {
                return Err(iae(format!("negative timeout {timeout_ms}")));
            }
            let (host, port) = read_inet_socket_address(ctx, sa)?;
            re1_connect_socket(ctx, this, &host, port, timeout_ms)
        },
    );

    r.register(sock, "bind", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(sa))) = args.get(1) {
            let (_, port) = read_inet_socket_address(ctx, *sa)?;
            sock_set(ctx, this, |s| s.local_port = port);
        }
        Ok(None)
    });

    r.register(sock, "isConnected", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = sock_get(ctx, this);
        Ok(Some(Value::Int(if s.stream_id >= 0 && s.closed == 0 {
            1
        } else {
            0
        })))
    });
    r.register(sock, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(if sock_get(ctx, this).closed != 0 {
            1
        } else {
            0
        })))
    });

    r.register(sock, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = sock_get(ctx, this).stream_id;
        if sid >= 0 {
            let mut reg = s2_registry().lock();
            if let Some(stream) = reg.streams.remove(&sid) {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
        sock_set(ctx, this, |s| {
            s.stream_id = -1;
            s.closed = 1;
        });
        Ok(None)
    });

    r.register(sock, "shutdownInput", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = sock_get(ctx, this).stream_id;
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                stream
                    .shutdown(std::net::Shutdown::Read)
                    .map_err(|e| ioex(format!("shutdown read failed: {e}")))?;
            } else if let Some(entry) = reg.tls_streams.get(&sid) {
                entry
                    .stream
                    .get_ref()
                    .shutdown(std::net::Shutdown::Read)
                    .map_err(|e| ioex(format!("shutdown read failed: {e}")))?;
            }
        }
        sock_set(ctx, this, |s| s.input_shutdown = 1);
        Ok(None)
    });
    r.register(sock, "shutdownOutput", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = sock_get(ctx, this).stream_id;
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                stream
                    .shutdown(std::net::Shutdown::Write)
                    .map_err(|e| ioex(format!("shutdown write failed: {e}")))?;
            } else if let Some(entry) = reg.tls_streams.get(&sid) {
                entry
                    .stream
                    .get_ref()
                    .shutdown(std::net::Shutdown::Write)
                    .map_err(|e| ioex(format!("shutdown write failed: {e}")))?;
            }
        }
        sock_set(ctx, this, |s| s.output_shutdown = 1);
        Ok(None)
    });
    // FIX (netty-client-socket-write-after-close residual): isInputShutdown/
    // isOutputShutdown had no reachable native registration in real-JDK mode
    // (the only registration lived in register_p72_server_socket, which is
    // only reachable via the synthetic-jdk-gated register_synthetic_overrides
    // umbrella — see the identical dead-code pattern already documented
    // elsewhere in this codebase, e.g. lib.rs's "FIX (httpserver-pkcs12
    // -20260706)" comment). Real bytecode ran instead, reading our synthetic
    // object's fields as if they were real Socket internals (`impl`/`shutIn`)
    // — undefined, and in practice non-deterministically truthy roughly one
    // run in three. Apache HttpClient5's DefaultBHttpClientConnection$1
    // .checkTLS() calls sslSocket.isInputShutdown() before every write and
    // throws ConnectionClosedException ("Connection is closed") if it
    // returns true, matching this bug's exact flaky signature (traced via
    // KRUN_STACK=1: ConnectionClosedException at
    // DefaultBHttpClientConnection$1.checkTLS, no CratonVM native or
    // exception involved at all up to that point — the socket was never
    // actually shut down).
    r.register(sock, "isInputShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(sock_get(ctx, this).input_shutdown)))
    });
    r.register(sock, "isOutputShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(sock_get(ctx, this).output_shutdown)))
    });

    r.register(sock, "setSoTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if ms < 0 {
            return Err(iae(format!("negative SO_TIMEOUT: {ms}")));
        }
        let sid = sock_get(ctx, this).stream_id;
        if sid >= 0 {
            let d = if ms == 0 {
                None
            } else {
                Some(Duration::from_millis(ms as u64))
            };
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                stream
                    .set_read_timeout(d)
                    .map_err(|e| ioex(format!("setSoTimeout failed: {e}")))?;
            } else if let Some(entry) = reg.tls_streams.get(&sid) {
                // FIX (netty-client-socket-write-after-close residual): a
                // socket connected via new13_do_create_socket's TLS path
                // (phases_late.rs) registers its stream id in `tls_streams`,
                // not `streams` — the table this native originally only
                // checked. Apache HttpClient5's `DefaultManagedHttpClient
                // Connection.bind()` calls `setSoTimeout`/`getSoTimeout`
                // unconditionally on every connection (see `getSoTimeout`'s
                // own doc comment below); missing `tls_streams` here made
                // both silently no-op for every TLS socket instead of
                // configuring the real underlying TcpStream's read timeout.
                entry
                    .stream
                    .get_ref()
                    .set_read_timeout(d)
                    .map_err(|e| ioex(format!("setSoTimeout failed: {e}")))?;
            }
        }
        sock_set(ctx, this, |s| s.read_timeout_ms = ms);
        Ok(None)
    });
    // `Socket.getSoTimeout()` had NO native override, so it fell through to
    // real bytecode: `Object o = getImpl().getOption(SO_TIMEOUT); ...`.
    // `getImpl()` reads the real private `impl` field — but our synthetic
    // `Socket` layout keeps the host STRING at field slot 0 (`SOCK_HOST`,
    // still needed by `getInetAddress()`), which collides with wherever
    // real `Socket`'s `impl` field happens to sit. `getImpl()` returned
    // that host string, and calling `.getOption(int)` on a `String`
    // produced `NoSuchMethodError: java/lang/String.getOption(I)...` —
    // breaking Apache HttpClient5's `DefaultManagedHttpClientConnection
    // .bind()`, which unconditionally calls `getSoTimeout()` on every new
    // connection. Query the real underlying `TcpStream`'s read timeout
    // (set by `setSoTimeout` above) directly instead of going through
    // `getImpl()` at all — same side-table-based approach `setSoTimeout`
    // already uses.
    r.register(sock, "getSoTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let configured = sock_get(ctx, this).read_timeout_ms;
        if configured != 0 {
            return Ok(Some(Value::Int(configured)));
        }
        let sid = sock_get(ctx, this).stream_id;
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let ms = stream
                    .read_timeout()
                    .map_err(|e| ioex(format!("getSoTimeout failed: {e}")))?
                    .map(|d| d.as_millis() as i32)
                    .unwrap_or(0);
                return Ok(Some(Value::Int(ms)));
            }
            // See the matching comment in setSoTimeout above: a TLS socket's
            // stream id lives in `tls_streams`, not `streams`.
            if let Some(entry) = reg.tls_streams.get(&sid) {
                let ms = entry
                    .stream
                    .get_ref()
                    .read_timeout()
                    .map_err(|e| ioex(format!("getSoTimeout failed: {e}")))?
                    .map(|d| d.as_millis() as i32)
                    .unwrap_or(0);
                return Ok(Some(Value::Int(ms)));
            }
        }
        Ok(Some(Value::Int(0)))
    });

    r.register(sock, "getPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(sock_get(ctx, this).port)))
    });
    r.register(sock, "getLocalPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(sock_get(ctx, this).local_port)))
    });
    r.register(
        sock,
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(addr) = re1_socket_adaptor_inet(ctx, this, false)? {
                return Ok(Some(Value::Object(Some(addr))));
            }
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
    // FIX (netty-client-socket-write-after-close): `getLocalAddress()` — the
    // LOCAL bind address — had no native registration at all (unlike its
    // sibling `getInetAddress()`, the REMOTE address, just above), so real
    // `java.net.Socket.getLocalAddress()` bytecode ran against this
    // synthetic object. That bytecode reads a real `SocketImpl`/holder
    // structure that doesn't exist here — this file's own `SOCK_HOST` slot
    // holds a plain `java.lang.String` instead (see the side-table
    // rationale above), so the real accessor it calls next resolves onto
    // `String` and throws a bogus `NoSuchMethodError:
    // java/lang/String.getOption(I)Ljava/lang/Object;` (observed via Apache
    // HttpClient5's connection setup calling this — see
    // docs/known-issues/netty-client-socket-write-after-close-nsme.md).
    // We don't track the real local bind IP for this client-side socket
    // (the TLS connect never does an explicit local bind), so return
    // loopback — a real client socket connecting to a loopback server
    // reports 127.0.0.1 as its local address too, and callers here only
    // need a non-crashing, non-null address (route/pool bookkeeping),
    // not byte-perfect network topology.
    r.register(
        sock,
        "getLocalAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(addr) = re1_socket_adaptor_inet(ctx, this, true)? {
                return Ok(Some(Value::Object(Some(addr))));
            }
            let ia = alloc_inet_address(ctx, "localhost", "127.0.0.1");
            Ok(Some(Value::Object(Some(ia))))
        },
    );

    r.register(
        sock,
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sid = sock_get(ctx, this).stream_id;
            if sid < 0 {
                return Err(ioex("Socket.getInputStream: not connected"));
            }
            let is = alloc_concurrent_synthetic(ctx, "java/net/Socket$SocketInputStream", 3);
            // Side-table the stream's owner+sid so we don't depend on field
            // layout (real `Socket$SocketInputStream` has different fields
            // than the synthetic shape: `parent:Socket`, `in:InputStream`).
            stream_owner_set(ctx, is, this);
            Ok(Some(Value::Object(Some(is))))
        },
    );
    r.register(
        sock,
        "getOutputStream",
        "()Ljava/io/OutputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sid = sock_get(ctx, this).stream_id;
            if sid < 0 {
                return Err(ioex("Socket.getOutputStream: not connected"));
            }
            let os = alloc_concurrent_synthetic(ctx, "java/net/Socket$SocketOutputStream", 3);
            stream_owner_set(ctx, os, this);
            Ok(Some(Value::Object(Some(os))))
        },
    );

    let sis = "java/net/Socket$SocketInputStream";
    r.register(
        sis,
        "read",
        "([BII)I",
        native_socket_input_stream_read_bytes,
    );
    // read([B)I — MUST be registered directly. Without it, `in.read(byte[])`
    // falls to the default java.io.InputStream.read(byte[]) bytecode, which
    // reads ONE byte then loops single-byte read() to fill the ENTIRE array,
    // blocking forever after the first record (e.g. it gets a 5-byte "PING\n"
    // into a 64-byte buffer then blocks waiting for byte 6 that never comes).
    // A single bulk read returning whatever is currently available (>=1 byte)
    // is the correct InputStream.read(byte[]) contract and unblocks every
    // server-side request read (okhttp MockWebServer, loopback HTTP). BUG-04.
    r.register(sis, "read", "([B)I", native_socket_input_stream_read_array);
    r.register(sis, "read", "()I", native_socket_input_stream_read_one);
    cratonvm_native_api::socket_input_stream_read::set_read_bytes(
        native_socket_input_stream_read_bytes,
    );
    cratonvm_native_api::socket_input_stream_read::set_read_array(
        native_socket_input_stream_read_array,
    );
    cratonvm_native_api::socket_input_stream_read::set_read_one(
        native_socket_input_stream_read_one,
    );
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
        let owner =
            stream_owner_get(ctx, this).ok_or_else(|| ioex("SocketOutputStream has no owner"))?;
        let buf = obj_arg(args, 1)?;
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        re1_socket_write_stream(ctx, owner, buf, off, len)
    });
    r.register(sos, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner =
            stream_owner_get(ctx, this).ok_or_else(|| ioex("SocketOutputStream has no owner"))?;
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) & 0xff;
        let one = ctx.new_array(ArrayElementType::Byte, 1);
        ctx.set_array_element(one, 0, Value::Int(b as i8 as i32));
        re1_socket_write_stream(ctx, owner, one, 0, 1)
    });
    r.register(sos, "flush", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(owner) = stream_owner_get(ctx, this) {
            let sid = sock_get(ctx, owner).stream_id;
            if sid >= 0 {
                let mut reg = s2_registry().lock();
                if let Some(stream) = reg.streams.get_mut(&sid) {
                    (&**stream)
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
    mut target: ObjectRef,
    timeout_ms: i32,
) -> MethodCallResult {
    if listener_id < 0 {
        return Err(ioex("ServerSocket not bound"));
    }
    // `target` is a Socket allocated by the enclosing ServerSocket.accept
    // native, not a Java-frame local or a safe_native_call argument. This method
    // can park in the poll loop below, where GC will scan only the deposited
    // blocked-thread snapshot. Keep the Socket in native_pin_roots for the whole
    // native so that snapshot includes it and any moving-GC fixup can be read
    // back before field writes and before returning it to Java.
    let target_pin = ctx.pin_native_root(target);
    // Poll-based accept. We deliberately do NOT block directly on a
    // `try_clone()`'d listener handle for the no-timeout case: on Windows,
    // closing one duplicated socket handle does not unblock a thread blocked in
    // `accept()` on another duplicate, so a `ServerSocket.close()` that drops
    // the registry's listener could never interrupt a blocked accept. okhttp's
    // MockWebServer relies on exactly that interruption — its accept runs on a
    // TaskRunner queue and `close()` waits up to 5 s for that queue to drain,
    // then throws `AssertionError: Gave up waiting for queue to shut down`
    // (it polluted every Spring HTTP-client test teardown).
    //
    // Instead we set the listener non-blocking and poll it, re-checking the
    // registry each iteration. When `close()` removes the listener (see
    // `re2_server_socket_close`), the next poll observes its absence and we
    // throw `SocketException("Socket closed")`, exactly as HotSpot's blocking
    // accept does on a closed ServerSocket. Each iteration holds the registry
    // lock only for the non-blocking accept syscall (never across a blocking
    // call), so the Hibernate-JTA bind/accept deadlock the old try_clone path
    // guarded against (registry lock held across a blocking accept) cannot
    // recur. `timeout_ms <= 0` means block indefinitely (until a connection
    // arrives or the socket is closed); `timeout_ms > 0` honours SO_TIMEOUT.
    enum AcceptOutcome {
        Accepted((TcpStream, SocketAddr)),
        Closed,
        TimedOut,
        Failed(std::io::Error),
    }
    let deadline = (timeout_ms > 0)
        .then(|| std::time::Instant::now() + Duration::from_millis(timeout_ms as u64));
    let outcome = loop {
        {
            let mut reg = s2_registry().lock();
            match reg.listeners.get_mut(&listener_id) {
                Some(listener) => {
                    // Idempotent + cheap; keeps the socket pollable.
                    listener.set_nonblocking(true).ok();
                    match listener.accept() {
                        Ok(pair) => break AcceptOutcome::Accepted(pair),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => break AcceptOutcome::Failed(e),
                    }
                }
                // Listener gone from the registry => another thread called
                // ServerSocket.close(). Mirror HotSpot: throw SocketException.
                None => break AcceptOutcome::Closed,
            }
        }
        if let Some(dl) = deadline {
            if std::time::Instant::now() >= dl {
                break AcceptOutcome::TimedOut;
            }
        }
        // GC-barrier safepoint gap: unlike every other blocking-native call in
        // this codebase (`Thread.sleep`, socket read/write), this poll-sleep
        // ran unmarked. A no-timeout `ServerSocket.accept()` (timeout_ms <= 0,
        // `deadline` is `None`) loops here indefinitely once no new
        // connections are pending, staying counted in the GC barrier's
        // `expected` set forever — it never reaches an interpreter safepoint
        // (it's native Rust code, not JIT either, so the cross-thread JIT
        // takeover can't rescue it), so any STW GC/JIT-takeover initiated
        // while this thread has no pending connection deadlocks permanently
        // (`stw-census` showed `pending=1 taken=0` unable to move). Mark each
        // poll-sleep slice as a blocked region, exactly like `Thread.sleep`'s
        // pump loop.
        //
        // `target` is a raw `ObjectRef` local held across the whole loop (it
        // is written to after a connection is accepted) — use the `_refs`
        // end-region variant so a moving GC that runs while we're blocked
        // rewrites it, same as `re1_socket_read_stream`'s `buf`.
        let mut blocked_refs = [Value::Object(Some(ctx.read_native_pin(target_pin, target)))];
        ctx.begin_blocking_region();
        std::thread::sleep(Duration::from_millis(10));
        ctx.end_blocking_region_refs(&mut blocked_refs);
        target = match blocked_refs[0] {
            Value::Object(Some(o)) => o,
            _ => target,
        };
        target = ctx.read_native_pin(target_pin, target);
    };

    // Restore blocking mode on the shared listener if it survived, so later
    // accept/getLocalSocketAddress calls see the conventional state.
    {
        let mut reg = s2_registry().lock();
        if let Some(l) = reg.listeners.get_mut(&listener_id) {
            l.set_nonblocking(false).ok();
        }
    }

    let (stream, peer) = match outcome {
        AcceptOutcome::Accepted(pair) => pair,
        AcceptOutcome::Closed => {
            // A real, catchable java.net.SocketException so MockWebServer's
            // `catch (SocketException)` accept loop terminates cleanly (and its
            // okhttp TaskRunner queue goes idle) instead of timing out.
            let jmsg = ctx.create_string("Socket closed");
            return match ctx.new_object_initialized(
                "java/net/SocketException",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(jmsg))],
            ) {
                Ok(Some(Value::Object(Some(exc)))) => {
                    // Keep the native-thrown exception rooted until
                    // safe_native_call can publish it as native_pending_return.
                    // The caller's Java frame has no catch-local root yet, and
                    // new_object_initialized releases its constructor pin before
                    // returning here.
                    let exc_pin = ctx.pin_native_root(exc);
                    let exc = ctx.read_native_pin(exc_pin, exc);
                    Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                        exc,
                    ))
                }
                _ => Err(ioex("ServerSocket.accept: Socket closed")),
            };
        }
        AcceptOutcome::TimedOut => {
            return Err(ioex("ServerSocket.accept failed: accept timed out"));
        }
        AcceptOutcome::Failed(e) => {
            return Err(ioex(format!("ServerSocket.accept failed: {e}")));
        }
    };
    // Force the accepted stream to blocking so a server-side read() actually
    // waits for data (and is woken by it) instead of returning WouldBlock or
    // never signalling. (BUG-04)
    let _ = stream.set_nonblocking(false);
    let peer_port = peer.port() as i32;
    let peer_ip = peer.ip().to_string();
    let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
    if std::env::var_os("CRATONVM_DBG_SOCK").is_some() {
        eprintln!(
            "[dbg-sock] accept: peer={peer_ip}:{peer_port} local_port={local_port} nonblocking_reset"
        );
    }
    let stream_id = s2_alloc_stream(stream);
    // GC-safety: `target` is a raw ObjectRef local that survived the poll
    // loop above (already fixed via blocked_refs there), but this is a
    // SEPARATE, later hazard -- `create_string` is a re-entrant, allocating
    // call (it can allocate the String object and trigger a GC), and nothing
    // protected `target` across it. A moving GC here relocates/reclaims
    // `target` before the very next line's `set_field` uses it, corrupting
    // the just-accepted Socket (observed: TestJNDIRealmIntegration's "LDAP
    // Listener Thread" — a plain `Socket s = accept()` local, stored and
    // read back across nothing but an unconditional `goto`, going stale;
    // `Socket.getInetAddress()` then read a zeroed field and returned null).
    // Pin it exactly like every other raw-ObjectRef-across-a-reentrant-call
    // site in this codebase.
    target = ctx.read_native_pin(target_pin, target);
    let host_str = ctx.create_string(&peer_ip);
    let target = ctx.read_native_pin(target_pin, target);
    ctx.set_field(target, SOCK_HOST, Value::Object(Some(host_str)));
    sock_set(ctx, target, |s| {
        s.port = peer_port;
        s.local_port = local_port;
        s.closed = 0;
        s.stream_id = stream_id;
    });
    // Leave `target_pin` live until `safe_native_call` installs
    // `native_pending_return` and truncates the native pin stack. Unpinning here
    // would recreate the native-return handoff window this path is protecting.
    Ok(Some(Value::Object(Some(target))))
}

fn re2_bind_listener(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    host: &str,
    port: i32,
    backlog: i32,
) -> MethodCallResult {
    let ip = resolve_host(host)?;
    let addr = SocketAddr::new(ip, port.clamp(0, 65535) as u16);
    let listener =
        TcpListener::bind(addr).map_err(|e| ioex(format!("BindException: {addr}: {e}")))?;
    let local_addr = listener.local_addr().ok();
    let actual_port = local_addr.map(|a| a.port() as i32).unwrap_or(port);
    let actual_host = local_addr
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| ip.to_string());
    let listener_id = s2_alloc_listener(listener);
    ss_set(ctx, this, |s| {
        s.port = actual_port;
        s.backlog = backlog.max(0);
        s.closed = 0;
        s.listener_id = listener_id;
    });
    // Publish the actual bound port to the cross-crate identity-keyed registry. The
    // re2 side-table above is private to native-builtins, but the last-registered (and
    // therefore winning) `getLocalPort` native lives in the sibling native-io crate
    // (socket_channel `ss_wrapper_local_port`) and shadows ALL ServerSocket dispatch.
    // It cannot see our side-table, and an int written to object field 0 does NOT
    // round-trip (the real ServerSocket layout's low slots are reference-typed). The
    // shared native-api table (keyed by GC-stable identity hash) is the channel that
    // lets that winner return the real ephemeral port instead of 0 — without which
    // `new ServerSocket(0).getLocalPort()` is 0 and any connect-to-advertised-port
    // (Narayana's TransactionStatusManager recovery listener) fails / hangs.
    cratonvm_native_api::server_socket_ports::record_addr(
        ctx.identity_hash_code(this),
        this,
        &actual_host,
        actual_port,
    );
    Ok(None)
}

/// Plain `java.net.ServerSocket.bind(SocketAddress[, int])` handler. Handles
/// both arities (backlog read from `args[2]` when present). Registered for both
/// descriptors below AND installed as the cross-crate plain-bind hook
/// ([`cratonvm_native_api::plain_server_socket_bind`]) so native-io's *winning*
/// `ss_wrapper_bind` (which shadows this registration) delegates the
/// no-channel-back-ref (plain `new ServerSocket()`) case back here instead of
/// no-opping — without which `new ServerSocket().bind(addr)` never bound a
/// listener and `getLocalPort()` stayed 0 (okhttp MockWebServer → port 0; BUG-04).
fn re2_server_socket_bind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let sa = obj_arg(args, 1).map_err(|_| ioex("bind: null address"))?;
    let (host, port) = read_inet_socket_address(ctx, sa)?;
    let backlog = args
        .get(2)
        .and_then(|v| v.as_int())
        .unwrap_or_else(|| ss_get(ctx, this).backlog);
    re2_bind_listener(ctx, this, &host, port, backlog)
}

/// `java.net.ServerSocket.close()` for the synthetic re2 surface. Drops the
/// listener from the shared `s2` registry (so the accept poll loop in
/// [`re2_accept_into`] observes the absence and throws `SocketException`,
/// matching HotSpot's blocked-accept-on-close) and clears the SO_TIMEOUT.
///
/// Also installed as the cross-crate plain-close hook
/// ([`cratonvm_native_api::plain_server_socket_close`]) so native-io's *winning*
/// `ss_wrapper_close` (which shadows this registration) delegates the
/// no-channel-back-ref (plain `new ServerSocket()`) case back here instead of
/// no-opping. Without this, a plain `ServerSocket.close()` released nothing, so
/// the listener stayed registered and a blocked accept never woke: okhttp's
/// `MockWebServer.close()` waits up to 5 s for its accept TaskRunner queue to
/// drain on teardown, then throws
/// `AssertionError: Gave up waiting for queue to shut down` (BUG: it polluted
/// every Spring HTTP-client test teardown). Mirrors the BUG-04 plain-bind hook.
fn re2_server_socket_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let lid = ss_get(ctx, this).listener_id;
    if lid >= 0 {
        s2_registry().lock().listeners.remove(&lid);
        re2_clear_accept_timeout(lid);
    }
    ss_set(ctx, this, |s| {
        s.closed = 1;
        s.listener_id = -1;
    });
    cratonvm_native_api::server_socket_ports::remove(ctx.identity_hash_code(this), this);
    Ok(None)
}

fn register_re2_server_socket(r: &mut NativeMethodRegistry) {
    // NIO-SERVER-SOCKET (route 1): skip the synthetic java.net.ServerSocket
    // surface so real bytecode drives sun/nio/ch/Net. See
    // register_phase53_socket_stubs.
    if std::env::var_os("CRATONVM_REAL_NET_SOCKETS").is_some() {
        return;
    }
    // Install the plain-`ServerSocket` bind hook for native-io's winning
    // `ss_wrapper_bind` to delegate to (BUG-04). Done unconditionally here (the
    // early-return above is the REAL_NET_SOCKETS path where native-io also defers
    // to real bytecode, so the hook is simply never consulted).
    cratonvm_native_api::plain_server_socket_bind::set(re2_server_socket_bind);
    // Companion plain-close hook (see re2_server_socket_close): native-io's
    // winning ss_wrapper_close delegates the plain ServerSocket case here so
    // close() actually drops the listener and wakes a blocked accept().
    cratonvm_native_api::plain_server_socket_close::set(re2_server_socket_close);
    let ss = "java/net/ServerSocket";

    r.register(ss, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // This native bypasses ServerSocket's field initializers.  Preserve
        // the real object's synchronization invariant before its bytecode
        // options path reaches getImpl().
        let this = re1_init_socket_locks(ctx, this);
        ss_set(ctx, this, |s| {
            s.port = -1;
            s.backlog = 50;
            s.closed = 0;
            s.listener_id = -1;
        });
        Ok(None)
    });

    r.register(ss, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let this = re1_init_socket_locks(ctx, this);
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        re2_bind_listener(ctx, this, "0.0.0.0", port, 50)
    });

    r.register(ss, "getLocalPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = ss_get(ctx, this).port;
        Ok(Some(Value::Int(p)))
    });

    r.register(ss, "<init>", "(II)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let this = re1_init_socket_locks(ctx, this);
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let backlog = args.get(2).and_then(|v| v.as_int()).unwrap_or(50);
        re2_bind_listener(ctx, this, "0.0.0.0", port, backlog)
    });

    r.register(ss, "<init>", "(IILjava/net/InetAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let this = re1_init_socket_locks(ctx, this);
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let backlog = args.get(2).and_then(|v| v.as_int()).unwrap_or(50);
        let host = match args.get(3) {
            Some(Value::Object(Some(ia))) => {
                inet_addr_field_string_or(ctx, *ia, IA_ADDR, "0.0.0.0")
            }
            _ => "0.0.0.0".to_string(),
        };
        let r = re2_bind_listener(ctx, this, &host, port, backlog);
        r
    });

    r.register(
        ss,
        "bind",
        "(Ljava/net/SocketAddress;)V",
        re2_server_socket_bind,
    );
    r.register(
        ss,
        "bind",
        "(Ljava/net/SocketAddress;I)V",
        re2_server_socket_bind,
    );

    r.register(ss, "accept", "()Ljava/net/Socket;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = ss_get(ctx, this);
        if s.closed != 0 {
            return Err(ioex("Socket is closed"));
        }
        let lid = s.listener_id;
        let timeout_ms = re2_accept_timeout_for(lid);
        let sock = alloc_concurrent_synthetic(ctx, "java/net/Socket", 5);
        ctx.set_field(sock, SOCK_HOST, Value::Object(None));
        sock_set(ctx, sock, |x| {
            x.port = 0;
            x.local_port = 0;
            x.closed = 0;
            x.stream_id = -1;
        });
        let sock = re1_init_socket_locks(ctx, sock);
        re2_accept_into(ctx, lid, sock, timeout_ms)
    });

    r.register(ss, "setSoTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if ms < 0 {
            return Err(iae(format!("negative SO_TIMEOUT: {ms}")));
        }
        let lid = ss_get(ctx, this).listener_id;
        re2_set_accept_timeout(lid, ms);
        Ok(None)
    });
    r.register(ss, "getSoTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ss_get(ctx, this).listener_id;
        Ok(Some(Value::Int(re2_accept_timeout_for(lid))))
    });

    // setReuseAddress / getReuseAddress: the synthetic `ServerSocket` has no
    // real `SocketImpl`, so the JDK bytecode for these (`getImpl().setOption(
    // SO_REUSEADDR, …)`) would NPE — `getImpl()` does `synchronized
    // (socketLock)` on a `socketLock` the synthetic `<init>` never initialises
    // (`NullPointerException: monitorenter in ServerSocket.getImpl`). This bites
    // WildFly's managed-container port check (`isPortAvailable` →
    // `new ServerSocket(port)` then `setReuseAddress(true)`). Service them as
    // no-ops: the stub listener does not model SO_REUSEADDR. Default to the
    // common `ServerSocket` value (true) for the getter.
    r.register(ss, "setReuseAddress", "(Z)V", |_ctx, _args| Ok(None));
    r.register(ss, "getReuseAddress", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });

    // The RE2 constructors own listener state outside the real ServerSocket
    // implementation.  JGroups configures this option before bind, where it
    // must be accepted without entering the real getImpl() bytecode path.
    r.register(ss, "setReceiveBufferSize", "(I)V", |_ctx, args| {
        let size = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if size <= 0 {
            return Err(iae(format!("negative receive buffer size: {size}")));
        }
        Ok(None)
    });
    r.register(ss, "getReceiveBufferSize", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(8192)))
    });
    r.register(ss, "close", "()V", re2_server_socket_close);

    r.register(ss, "isBound", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ss_get(ctx, this).listener_id;
        Ok(Some(Value::Int(if lid >= 0 { 1 } else { 0 })))
    });
    r.register(ss, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ss_get(ctx, this).closed)))
    });

    r.register(
        ss,
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let lid = ss_get(ctx, this).listener_id;
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
                Some((ip, port)) => Ok(Some(Value::Object(Some(
                    alloc_inet_socket_address_resolved(ctx, &ip, &ip, port),
                )))),
                None => Ok(Some(Value::Object(None))),
            }
        },
    );

    // `getInetAddress()` — the bound local address. This was previously
    // unregistered, so it fell through to the real `ServerSocket.getInetAddress`
    // bytecode (`if (!isBound()) return null; …`) which returns **null** for the
    // synthetic socket. Narayana's `TxControl.<clinit>` does
    // `serverSocket.getInetAddress().getHostAddress()`, so that null NPEs and
    // aborts every JTA-platform test (HIB-DEV-02). Resolve the bound IP from the
    // shared listener registry — checking both the re2 side-table and the
    // object-field listener id used by the phase-53 ServerSocket ctors (the two
    // synthetic surfaces coexist; see `reference_server_socket_gap`) — and fall
    // back to the wildcard address so the caller gets a usable, non-null
    // InetAddress (matching `new ServerSocket(port).getInetAddress()` == 0.0.0.0
    // on HotSpot) instead of an NPE.
    r.register(
        ss,
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mut lid = ss_get(ctx, this).listener_id;
            if lid < 0 {
                lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
            }
            let ip = if lid >= 0 {
                let reg = s2_registry().lock();
                reg.listeners
                    .get(&lid)
                    .and_then(|l| l.local_addr().ok())
                    .map(|a| a.ip().to_string())
            } else {
                None
            };
            let ip = ip.unwrap_or_else(|| "0.0.0.0".to_string());
            let ia = alloc_inet_address(ctx, &ip, &ip);
            Ok(Some(Value::Object(Some(ia))))
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
            let name = if host.is_empty() {
                "localhost".to_string()
            } else {
                host
            };
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
                    return Err(uhex(format!("{host}: {e}")));
                }
            }
            if addrs.is_empty() {
                return Err(uhex(format!("{host}")));
            }
            let arr = ctx.new_ref_array(ClassId::new(0), addrs.len());
            let name = if host.is_empty() {
                "localhost".to_string()
            } else {
                host
            };
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
        Ok(Some(inet_addr_field(ctx, this, IA_ADDR)))
    });
    r.register(ia, "getHostName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(inet_addr_field(ctx, this, IA_HOST)))
    });
    r.register(
        ia,
        "getCanonicalHostName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(inet_addr_field(ctx, this, IA_HOST)))
        },
    );
    r.register(ia, "getAddress", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip_str = inet_addr_field_string_or(ctx, this, IA_ADDR, "0.0.0.0");
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
        let ip_str = inet_addr_field_string_or(ctx, this, IA_ADDR, "");
        let is_lb = ip_str
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
        Ok(Some(Value::Int(if is_lb { 1 } else { 0 })))
    });
    r.register(ia, "isAnyLocalAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip_str = inet_addr_field_string_or(ctx, this, IA_ADDR, "");
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
        let ip_str = inet_addr_field_string_or(ctx, this, IA_ADDR, "");
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

    // `equals`, `hashCode`, `toString` — registered for `InetAddress` and
    // both concrete subclasses below. Real-JDK implements these by reading
    // the `holder`; routing them through the layout-aware reader keeps a
    // CratonVM-synthesised mirror consistent with whatever the application
    // (e.g. Hazelcast's `DefaultAddressPicker`, which keys cluster members
    // by `InetAddress`) expects.
    register_inet_address_object_methods(r, ia);

    for cls in ["java/net/Inet4Address", "java/net/Inet6Address"] {
        r.register(
            cls,
            "getHostAddress",
            "()Ljava/lang/String;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(inet_addr_field(ctx, this, IA_ADDR)))
            },
        );
        r.register(cls, "getHostName", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(inet_addr_field(ctx, this, IA_HOST)))
        });
        r.register(
            cls,
            "getCanonicalHostName",
            "()Ljava/lang/String;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(inet_addr_field(ctx, this, IA_HOST)))
            },
        );
        r.register(cls, "getAddress", "()[B", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Object(Some(inet_addr_address_bytes(
                ctx, this,
            )))))
        });
        r.register(cls, "isLoopbackAddress", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(inet_addr_predicate(
                ctx,
                this,
                |ip| ip.is_loopback(),
            )))))
        });
        r.register(cls, "isAnyLocalAddress", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(inet_addr_predicate(
                ctx,
                this,
                |ip| match ip {
                    IpAddr::V4(v) => v.is_unspecified(),
                    IpAddr::V6(v) => v.is_unspecified(),
                },
            )))))
        });
        r.register(cls, "isMulticastAddress", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(inet_addr_predicate(
                ctx,
                this,
                |ip| ip.is_multicast(),
            )))))
        });
        register_inet_address_object_methods(r, cls);
    }
}

/// Parse the `IA_ADDR` field of an InetAddress mirror and apply `pred`.
/// Used by the `isLoopbackAddress` / `isAnyLocalAddress` / `isMulticastAddress`
/// predicate natives shared across `InetAddress` and its subclasses.
fn inet_addr_predicate(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    pred: impl Fn(IpAddr) -> bool,
) -> bool {
    inet_addr_field_string_or(ctx, this, IA_ADDR, "")
        .parse::<IpAddr>()
        .map(pred)
        .unwrap_or(false)
}

/// Raw-bytes form of an InetAddress mirror's IP (4 bytes IPv4 / 16 IPv6).
fn inet_addr_address_bytes(ctx: &mut dyn NativeContext, this: ObjectRef) -> ObjectRef {
    let ip_str = inet_addr_field_string_or(ctx, this, IA_ADDR, "0.0.0.0");
    let bytes: Vec<u8> = if let Ok(v4) = ip_str.parse::<Ipv4Addr>() {
        v4.octets().to_vec()
    } else if let Ok(v6) = ip_str.parse::<Ipv6Addr>() {
        v6.octets().to_vec()
    } else {
        vec![0u8; 4]
    };
    new_java_byte_array(ctx, &bytes)
}

/// Register `equals(Object)`, `hashCode()`, `toString()` for an InetAddress
/// class. These mirror real-JDK semantics (equality by address bytes, hash
/// of the address int, `toString` = `"hostName/ipAddress"`) but read state
/// through the layout-aware [`inet_addr_resolve`] reader so they work on
/// CratonVM-synthesised mirrors whose host/IP lives in the side table.
fn register_inet_address_object_methods(r: &mut NativeMethodRegistry, cls: &'static str) {
    r.register(cls, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let host = inet_addr_field_string_or(ctx, this, IA_HOST, "");
        let ip = inet_addr_field_string_or(ctx, this, IA_ADDR, "");
        // Real-JDK `InetAddress.toString()` => `hostName + "/" + ipString`.
        let s = ctx.create_string(&format!("{host}/{ip}"));
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(cls, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip = inet_addr_field_string_or(ctx, this, IA_ADDR, "0.0.0.0");
        // Real-JDK `Inet4Address.hashCode()` returns the packed address int.
        let h = match ip.parse::<IpAddr>() {
            Ok(IpAddr::V4(v4)) => i32::from_be_bytes(v4.octets()),
            Ok(IpAddr::V6(v6)) => {
                let o = v6.octets();
                let mut h = 0i32;
                for chunk in o.chunks(4) {
                    h ^= i32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                }
                h
            }
            Err(_) => 0,
        };
        Ok(Some(Value::Int(h)))
    });
    r.register(cls, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        // Real-JDK `InetAddress.equals` compares the address bytes.
        let a = inet_addr_field_string_or(ctx, this, IA_ADDR, "");
        let b = inet_addr_field_string_or(ctx, other, IA_ADDR, "");
        let eq = !a.is_empty() && a.parse::<IpAddr>().ok() == b.parse::<IpAddr>().ok();
        Ok(Some(Value::Int(i32::from(eq))))
    });
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

/// Parse an `http(s)://[userinfo@]host[:port][/path]` URL into
/// `(https, host, port, path, userinfo)`. The user-info (if any) is stripped
/// from the connect target / Host header and returned separately so callers
/// can turn it into preemptive `Authorization: Basic` credentials (the
/// real-JDK `HttpURLConnection` behaviour Spring's
/// `ResourceTests.useUserInfoToSetBasicAuth` relies on).
fn http_parse_url(url: &str) -> Result<(bool, String, u16, String, Option<String>), String> {
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
    // RFC 3986: authority = [ userinfo "@" ] host [ ":" port ]. Split at the
    // LAST '@' (userinfo may itself contain an encoded/raw '@').
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (Some(authority[..i].to_string()), &authority[i + 1..]),
        None => (None, authority),
    };
    let (host, port) = match hostport.rfind(':') {
        Some(i) => {
            let (h, p) = (&hostport[..i], &hostport[i + 1..]);
            let pn: u16 = p.parse().map_err(|_| format!("bad port in {url}"))?;
            (h.to_string(), pn)
        }
        None => (hostport.to_string(), if scheme { 443 } else { 80 }),
    };
    Ok((scheme, host, port, path.to_string(), userinfo))
}

fn http_perform_request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    max_redirects: usize,
) -> std::io::Result<HttpResponse> {
    http_perform_request_with_timeout(
        method,
        url,
        headers,
        body,
        Duration::from_secs(30),
        max_redirects,
    )
}

/// Performs one logical HTTP request with an end-to-end deadline.  Redirects
/// share the same deadline: `HttpRequest.timeout(Duration)` is a timeout for
/// the request, not a fresh 30-second allowance for every hop.
fn http_perform_request_with_timeout(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
    max_redirects: usize,
) -> std::io::Result<HttpResponse> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "request timed out"))?;
    let mut current_url = url.to_string();
    let mut current_method = method.to_string();
    let mut current_body = body.to_vec();
    for _ in 0..=max_redirects {
        let (https, host, port, path, userinfo) = http_parse_url(&current_url)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        // JDK parity: a URL carrying user-info (`http://alice:secret@host/…`)
        // sends preemptive `Authorization: Basic base64(userinfo)` unless the
        // caller already staged an explicit Authorization header
        // (sun.net.www.protocol.http.HttpURLConnection does this from
        // url.getUserInfo(); asserted by ResourceTests.useUserInfoToSetBasicAuth).
        let hdrs_with_auth: Vec<(String, String)>;
        let eff_headers: &[(String, String)] = match userinfo {
            Some(ui)
                if !headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("authorization")) =>
            {
                let b64 = String::from_utf8(crate::b64_encode(ui.as_bytes(), 0, false))
                    .unwrap_or_default();
                let mut v = headers.to_vec();
                v.push(("Authorization".to_string(), format!("Basic {b64}")));
                hdrs_with_auth = v;
                &hdrs_with_auth
            }
            _ => headers,
        };
        let resp = if https {
            http_exchange_tls(
                &host,
                port,
                &path,
                &current_method,
                eff_headers,
                &current_body,
                deadline,
            )?
        } else {
            http_exchange_plain(
                &host,
                port,
                &path,
                &current_method,
                eff_headers,
                &current_body,
                deadline,
            )?
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
                        let (scheme, h, p, _, _) = http_parse_url(&current_url).map_err(|e| {
                            std::io::Error::new(std::io::ErrorKind::InvalidInput, e)
                        })?;
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
    let mut has_user_agent = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") {
            has_content_length = true;
        }
        if k.eq_ignore_ascii_case("user-agent") {
            has_user_agent = true;
        }
        let _ = write!(&mut out, "{k}: {v}\r\n");
    }
    if !has_user_agent {
        out.extend_from_slice(b"User-Agent: cratonvm-phaseE/1.0\r\n");
    }
    if !has_content_length && (!body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH")) {
        let _ = write!(&mut out, "Content-Length: {}\r\n", body.len());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

/// Real HTTP/1.1 response reader: reads the header block, then reads the
/// body according to the framing the headers actually declare
/// (`Content-Length`, chunked, or close-delimited), stopping as soon as the
/// message is complete instead of unconditionally reading until the peer
/// closes the connection.
///
/// A real HTTP/1.1 peer is entitled to keep a connection open after sending
/// a fully-framed response (keep-alive) -- reading until EOF unconditionally
/// hangs forever on such a connection even though the whole response
/// already arrived. See
/// docs/known-issues/spring-web-flow-outputstreamwriter-close-corruption.md
/// root cause #2 (`JdkClientHttpRequestFactoryTests` hang): confirmed via a
/// live `strace` against a real `MockWebServer` that the server sent a
/// complete 38-byte `Content-Length`-framed response and went straight back
/// to `recv()` waiting for the next keep-alive request, while this client
/// kept `recv()`-ing for a close that was never coming.
///
/// `head_response` must be `true` when the request that elicited this
/// response was a HEAD: RFC 9110 §9.3.2 entitles a HEAD response to carry
/// the same framing headers (`Content-Length`/`Transfer-Encoding`) the
/// corresponding GET would have, while sending NO body bytes at all --
/// waiting for the declared `Content-Length` blocks on data the server will
/// never send until the socket's `SO_RCVTIMEO` fires (`IOException:
/// Resource temporarily unavailable (os error 11)`,
/// `JdkClientHttpRequestFactoryTests.gzipCompressionWithHeadRequest`).
fn http_read_response<R: Read>(mut r: R, head_response: bool) -> std::io::Result<HttpResponse> {
    let mut all = Vec::with_capacity(8192);
    let mut buf = [0u8; 4096];

    // Phase 1: read until the response headers are fully present.
    let sep = loop {
        if let Some(pos) = all.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        match r.read(&mut buf) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed before response headers were complete",
                ));
            }
            Ok(n) => all.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    };

    let header_block = &all[..sep];
    let head_str = std::str::from_utf8(header_block)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut lines = head_str.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing status"))?;
    let mut parts = status_line.splitn(3, ' ');
    let _ = parts.next();
    let code_str = parts.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing status code")
    })?;
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

    // Phase 2: read the body per the declared framing, stopping as soon as
    // it's complete.
    let body_start = sep + 4;
    // 1xx/204/304 never carry a body regardless of the framing headers, and
    // neither does any response to a HEAD request.
    let no_body = head_response || matches!(status, 100..=199 | 204 | 304);

    let body = if no_body {
        Vec::new()
    } else if chunked {
        // Mirrors the server-side chunked-body read loop elsewhere in this
        // file (`http_decode_chunked` returns `Ok` only once the terminating
        // zero-size chunk is present): read more whenever it still errors,
        // until it succeeds or the peer closes/errors.
        loop {
            match http_decode_chunked(&all[body_start..]) {
                Ok(b) => break b,
                Err(_) => match r.read(&mut buf) {
                    Ok(0) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "connection closed before chunked body was complete",
                        ));
                    }
                    Ok(n) => all.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                },
            }
        }
    } else if let Some(n) = content_length {
        while all.len() - body_start < n {
            match r.read(&mut buf) {
                // Peer closed early: return whatever body arrived, matching
                // the previous read-until-EOF behavior's leniency for a
                // short response instead of erroring.
                Ok(0) => break,
                Ok(read) => all.extend_from_slice(&buf[..read]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let body_region = &all[body_start..];
        body_region[..body_region.len().min(n)].to_vec()
    } else {
        // Neither Content-Length nor chunked: the only remaining HTTP/1.1
        // framing is "read until the connection closes" (legacy
        // close-delimited body) -- a compliant server without either header
        // MUST close the connection to signal the end of the body, so
        // waiting for EOF here is still correct and necessary.
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        all[body_start..].to_vec()
    };

    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn http_decode_chunked(mut data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    loop {
        let nl = data.windows(2).position(|w| w == b"\r\n").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk header")
        })?;
        let size_str = std::str::from_utf8(&data[..nl])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let size_str = size_str.split(';').next().unwrap_or("0").trim();
        // FIX(net-phase-e #1): parse the hex chunk-length defensively.
        // `usize::from_str_radix` itself errors (not panics) on numeric
        // overflow, but a crafted-but-in-range giant size still drove
        // `n + 2` to overflow (panic in debug builds) and `&data[..n]`
        // to slice out of range (panic). Reject empty/non-hex sizes, and
        // cap the chunk at a sane maximum so the subsequent arithmetic and
        // slicing can never overflow or wrap — on any violation return a
        // protocol error instead of panicking.
        const MAX_CHUNK: usize = 64 * 1024 * 1024; // 64 MiB hard cap per chunk
        if size_str.is_empty() || !size_str.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bad chunk size",
            ));
        }
        let n = usize::from_str_radix(size_str, 16)
            .ok()
            .filter(|&n| n <= MAX_CHUNK)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "chunk size too large")
            })?;
        data = &data[nl + 2..];
        if n == 0 {
            break;
        }
        // `n <= MAX_CHUNK` guarantees `n + 2` cannot overflow `usize`.
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

fn re5_dbg() -> bool {
    std::env::var_os("CRATONVM_DBG_RE5").is_some()
}

fn http_timeout_remaining(deadline: Instant) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "request timed out"))
}

fn http_connect_with_deadline(
    host: &str,
    port: u16,
    deadline: Instant,
) -> std::io::Result<TcpStream> {
    let mut last_error = None;
    for address in (host, port).to_socket_addrs()? {
        let remaining = http_timeout_remaining(deadline)?;
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "HTTP host resolved to no addresses",
        )
    }))
}

trait HttpDeadlineSocket: Read {
    fn set_http_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()>;
}

impl HttpDeadlineSocket for TcpStream {
    fn set_http_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.set_read_timeout(timeout)
    }
}

impl HttpDeadlineSocket for native_tls::TlsStream<TcpStream> {
    fn set_http_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.get_ref().set_read_timeout(timeout)
    }
}

/// Re-arms the operating-system receive timeout before every response read so
/// a peer that drips bytes cannot extend a request deadline indefinitely.
struct HttpDeadlineReader<S> {
    stream: S,
    deadline: Instant,
}

impl<S: HttpDeadlineSocket> Read for HttpDeadlineReader<S> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.stream
            .set_http_read_timeout(Some(http_timeout_remaining(self.deadline)?))?;
        match self.stream.read(buffer) {
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "request timed out",
                ))
            }
            other => other,
        }
    }
}

fn http_exchange_plain(
    host: &str,
    port: u16,
    path: &str,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
    deadline: Instant,
) -> std::io::Result<HttpResponse> {
    let dbg = re5_dbg();
    let t0 = if dbg { Some(Instant::now()) } else { None };
    if dbg {
        eprintln!("[RE5-DBG] {method} {host}:{port}{path} connecting...");
    }
    let connect_result = http_connect_with_deadline(host, port, deadline);
    if dbg {
        eprintln!(
            "[RE5-DBG] {method} {host}:{port}{path} connect -> {:?} ({:?} elapsed)",
            connect_result.as_ref().map(|_| "OK").map_err(|e| e.kind()),
            t0.map(|t| t.elapsed())
        );
    }
    let mut stream = connect_result?;
    stream.set_write_timeout(Some(http_timeout_remaining(deadline)?))?;
    let req = http_build_request(method, host, port, path, headers, body, 80);
    if dbg {
        eprintln!(
            "[RE5-DBG] {method} {host}:{port}{path} writing {} bytes (body {} bytes): {:?}",
            req.len(),
            body.len(),
            String::from_utf8_lossy(&req[..req.len().min(200)])
        );
    }
    let write_result = stream.write_all(&req);
    if dbg {
        eprintln!(
            "[RE5-DBG] {method} {host}:{port}{path} write_all -> {:?} ({:?} elapsed)",
            write_result.as_ref().map(|_| "OK").map_err(|e| e.kind()),
            t0.map(|t| t.elapsed())
        );
    }
    write_result?;
    stream.flush()?;
    if dbg {
        eprintln!("[RE5-DBG] {method} {host}:{port}{path} flushed, reading response...");
    }
    let read_result = http_read_response(
        HttpDeadlineReader { stream, deadline },
        method.eq_ignore_ascii_case("HEAD"),
    );
    if dbg {
        eprintln!(
            "[RE5-DBG] {method} {host}:{port}{path} read_response -> status={:?} err={:?} ({:?} elapsed)",
            read_result.as_ref().ok().map(|r| r.status),
            read_result.as_ref().err().map(|e| e.kind()),
            t0.map(|t| t.elapsed())
        );
    }
    read_result
}

fn http_exchange_tls(
    host: &str,
    port: u16,
    path: &str,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
    deadline: Instant,
) -> std::io::Result<HttpResponse> {
    let connector = native_tls::TlsConnector::builder()
        .build()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("TLS init: {e}")))?;
    let tcp = http_connect_with_deadline(host, port, deadline)?;
    tcp.set_read_timeout(Some(http_timeout_remaining(deadline)?))?;
    tcp.set_write_timeout(Some(http_timeout_remaining(deadline)?))?;
    let mut tls = connector.connect(host, tcp).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::Other, format!("TLS handshake: {e}"))
    })?;
    let req = http_build_request(method, host, port, path, headers, body, 443);
    tls.write_all(&req)?;
    tls.flush()?;
    http_read_response(
        HttpDeadlineReader {
            stream: tls,
            deadline,
        },
        method.eq_ignore_ascii_case("HEAD"),
    )
}

fn huc_extract_req_headers(ctx: &dyn NativeContext, hdrs: Value) -> Vec<(String, String)> {
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

/// Return the originating URL's external form for one of our synthetic
/// HttpURLConnection carriers. Unlike `huc_url_string`, this deliberately uses
/// `toExternalForm`: a real-JDK URL's field zero is only its protocol (for
/// example, `file`), not a complete URL suitable for filesystem metadata.
fn huc_origin_url_string(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    let url = match ctx.get_field(this, HUC_URL) {
        Value::Object(Some(u)) => u,
        _ => return String::new(),
    };
    match ctx.invoke_virtual(url, "toExternalForm", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => huc_url_string(ctx, this),
    }
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

/// Resolve a `classpath:` resource through the current thread's context
/// classloader, mirroring Tomcat's real `ClasspathURLStreamHandler.openConnection`
/// (`Thread.currentThread().getContextClassLoader().getResourceAsStream(path)`).
///
/// `find_resource` only sees the static bootstrap/ext/app classpath; it cannot
/// reach a deployed webapp's `WEB-INF/classes`. The webapp's
/// `WebappClassLoaderBase` overrides `getResourceAsStream` and resolves those via
/// its `WebResourceRoot` (real Tomcat bytecode), so going through the live TCCL
/// picks them up. Returns the loader's own `InputStream` (already a real object),
/// or `None` when there is no context loader or it has no such resource — the
/// caller then falls back to `FileNotFoundException`, matching the real handler.
///
/// Ordering note: the loader ref is only held across a single small
/// `create_string` allocation before its `getResourceAsStream` call, keeping the
/// moving-GC exposure minimal (same shape as the `toExternalForm` hop above).
fn classpath_resource_via_context_loader(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Option<ObjectRef> {
    let thread = match ctx.invoke(
        "java/lang/Thread",
        "currentThread",
        "()Ljava/lang/Thread;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(t)))) => t,
        _ => return None,
    };
    let loader = match ctx.invoke_virtual(
        thread,
        "getContextClassLoader",
        "()Ljava/lang/ClassLoader;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(l)))) => l,
        _ => return None,
    };
    let name_s = ctx.create_string(name);
    match ctx.invoke_virtual(
        loader,
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        &[Value::Object(Some(name_s))],
    ) {
        Ok(Some(Value::Object(Some(stream)))) => Some(stream),
        _ => None,
    }
}

/// True when `s` is a usable *full URL* (carries a scheme), as opposed to a bare
/// `host:port` authority. A real `java.net.URL`'s field index 5 is the
/// `authority` (e.g. `"localhost:8080"`), NOT a synthetic full-URL cache — so a
/// plain `contains(':')` test wrongly accepts the authority as the URL, making
/// `toExternalForm()`/`openStream` see scheme `"localhost:8080"`
/// (`unsupported scheme: localhost:<port>`). Discriminator: in a real URL the
/// text after the FIRST ':' is `"//..."` or a path/opaque part; in an authority
/// it is the numeric port. So reject when everything after the first ':' is
/// ASCII digits (a port).
///
/// A real URL's authority may ALSO carry user-info (`alice:secret@localhost:8080`),
/// where the text after the first ':' is NOT all digits — the digits-only test
/// alone then misreads the authority as a full URL with scheme `alice`
/// (`URL.openStream: unsupported scheme: alice:secret@localhost:<port>`,
/// ResourceTests.useUserInfoToSetBasicAuth). Discriminator: an authority's
/// user-info '@' appears before any '/', while a full URL's first '/' comes
/// immediately after `scheme:` (e.g. `http://u:p@h/x` → `http:` precedes the
/// first '/', no '@' in it). So additionally reject when the segment before
/// the first '/' contains '@'.
fn field5_is_full_url(s: &str) -> bool {
    let before_slash = s.split('/').next().unwrap_or(s);
    if s.starts_with('[') {
        return false;
    }
    if before_slash.contains('@') {
        return false;
    }
    match s.split_once(':') {
        Some((scheme, rest)) => {
            !scheme.is_empty() && !rest.is_empty() && !rest.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
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
            // Only treat field 5 as a full-URL cache when it actually carries a
            // scheme — a real java.net.URL's field 5 is the `authority`
            // (`host:port`), which must fall through to the reconstruction below
            // instead of being returned as the whole URL. (BUG: openStream then
            // saw `unsupported scheme: localhost:<port>`.)
            if field5_is_full_url(s) {
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
        // Real java.net.URL.toExternalForm emits `protocol://authority` where
        // the authority (field 5) may carry user-info
        // (`alice:secret@localhost:8080`). Prefer it over the bare host:port
        // reconstruction so user-info survives (ResourceTests.
        // useUserInfoToSetBasicAuth needs the native HTTP client to see it and
        // send preemptive Basic auth). Guard: only when the parsed host is
        // non-empty and embedded in the field-5 string, so a synthetic 6-field
        // URL's unrelated slot never leaks in. Reaching here already implies
        // `field5_is_full_url(field 5)` was false (the fast path above
        // returned otherwise), i.e. field 5 is authority-shaped or empty.
        let authority_from_field5 = match &synth_full {
            Some(s) if !host.is_empty() && s.contains(&host) => Some(s.as_str()),
            _ => None,
        };
        if let Some(auth) = authority_from_field5 {
            out.push_str("//");
            out.push_str(auth);
        } else if !host.is_empty() || port >= 0 {
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
        if !field5_is_full_url(&url_str) {
            // Empty, just a protocol, or a real-JDK `authority` (host:port) in
            // field 5 — ask the URL for its full form (toExternalForm now
            // reconstructs protocol://authority/path for real URLs).
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
            //
            // `file:` URLs prefix a leading `/` before a Windows drive letter
            // (`/C:/…`), which must be stripped for `std::fs`/`Path` to resolve
            // the path on Windows. On POSIX that same leading `/` is the path's
            // own root and must be KEPT — unconditionally trimming every
            // leading slash turned `/home/user/foo.jar` into the relative
            // `home/user/foo.jar`, which resolved (if at all) against the
            // wrong CWD and failed with ENOENT on every Linux/macOS host. Try
            // the trimmed form first, then the raw form — mirrors the
            // existence-check fallback already used by `jar_url_entry_size`
            // and `JarURLConnection.getJarFile` below for the same ambiguity.
            let raw_rest = rest;
            let trimmed_rest = rest.trim_start_matches('/');
            let rest =
                if std::path::Path::new(trimmed_rest.split("!/").next().unwrap_or(trimmed_rest))
                    .exists()
                {
                    trimmed_rest
                } else {
                    raw_rest
                };
            let (outer_jar, inner_path) = match rest.find("!/") {
                Some(i) => (&rest[..i], &rest[i + 2..]),
                None => {
                    return Err(ioex(format!(
                        "URL.openStream: malformed jar URL: {url_str}"
                    )))
                }
            };
            // Check if the inner_path itself is a nested jar entry (double !/):
            // e.g. "BOOT-INF/lib/spring-boot-2.7.12.jar!/META-INF/spring.factories"
            let buf = if let Some(second_sep) = inner_path.find("!/") {
                let nested_jar_entry = &inner_path[..second_sep];
                let resource_entry = &inner_path[second_sep + 2..];
                use std::io::Read;
                let nested_jar_bytes =
                    cached_nested_jar(outer_jar, nested_jar_entry).map_err(|e| {
                        ioex(format!(
                            "URL.openStream: nested jar {nested_jar_entry} in {outer_jar}: {e}"
                        ))
                    })?;
                let inner_cursor = std::io::Cursor::new(nested_jar_bytes.as_slice());
                let mut inner_zip = zip::ZipArchive::new(inner_cursor).map_err(|e| {
                    ioex(format!(
                        "URL.openStream: open inner jar {nested_jar_entry}: {e}"
                    ))
                })?;
                let mut entry_file = inner_zip
                    .by_name(resource_entry)
                    .map_err(|e| zip_entry_err(resource_entry, nested_jar_entry, e))?;
                let mut buf = Vec::with_capacity(entry_file.size().min(1 << 27) as usize);
                entry_file.read_to_end(&mut buf).map_err(|e| {
                    ioex(format!("URL.openStream: read entry {resource_entry}: {e}"))
                })?;
                buf
            } else {
                // Single-level: jar:file:/path/to.jar!/entry
                use std::io::Read;
                let jar_bytes = cached_outer_jar(outer_jar)
                    .map_err(|e| ioex(format!("URL.openStream: read jar {outer_jar}: {e}")))?;
                let cursor = std::io::Cursor::new(jar_bytes.as_slice());
                let mut zip = zip::ZipArchive::new(cursor)
                    .map_err(|e| ioex(format!("URL.openStream: open jar {outer_jar}: {e}")))?;
                // A `.jmod` archive stores its class/resource entries under a
                // `classes/` prefix, but a `getResource` URL into a jmod omits
                // it (e.g. `…/java.base.jmod!/java/lang/Object.class`). Resolve
                // to the prefixed entry, mirroring `find_resource`'s jmod path.
                // Without this, Hibernate's Jandex indexer
                // (`SourceModelTestHelper.buildJandexIndex`, which reads JDK
                // baseline types out of `java.base.jmod`) failed with
                // "entry java/lang/Object.class … not found in archive"; it
                // also made `ClassLoader.getResourceAsStream("java/lang/...")`
                // null, blocking ecj/Jasper JSP type resolution (bug 10).
                let lookup: std::borrow::Cow<str> =
                    if outer_jar.ends_with(".jmod") && !inner_path.starts_with("classes/") {
                        std::borrow::Cow::Owned(format!("classes/{inner_path}"))
                    } else {
                        std::borrow::Cow::Borrowed(inner_path)
                    };
                let mut entry_file = zip
                    .by_name(&lookup)
                    .map_err(|e| zip_entry_err(inner_path, outer_jar, e))?;
                let mut buf = Vec::with_capacity(entry_file.size().min(1 << 27) as usize);
                entry_file
                    .read_to_end(&mut buf)
                    .map_err(|e| ioex(format!("URL.openStream: read entry {inner_path}: {e}")))?;
                buf
            };
            buf
        } else if let Some(raw_rest) = url_str.strip_prefix("file:") {
            // HotSpot's `sun.net.www.protocol.file.FileURLConnection` decodes
            // the URL's raw (percent-escaped) file component via
            // `ParseUtil.decode(url.getFile())` before touching the
            // filesystem, so a `file:` URL like
            // `file:/…/foo%20with%20spaces.css` (produced by
            // `UrlResource`/`UriUtils.encodePath`) resolves to the file
            // literally named "foo with spaces.css". We previously used the
            // still-encoded `raw_rest`/`path` verbatim, so `std::fs::read`
            // looked for a path containing a literal "%20" — always missing
            // — and any caller expecting content (e.g.
            // `ResourceHttpRequestHandlerIntegrationTests
            // .classpathLocationWithEncodedPath`, `pathPrefix="/url"`) got an
            // empty/404 response instead of the file's bytes. Decode once,
            // up front, the same way URI handling elsewhere in this file
            // does (`uri_percent_decode`).
            let rest_owned = uri_percent_decode(raw_rest);
            let rest = rest_owned.as_str();
            let path = rest.trim_start_matches('/');
            // On Windows, MSYS/Cygwin-style `/c/...` (URL `file:/c/...`)
            // needs the drive-letter colon reinjected: `c/foo` → `c:/foo`.
            // ActiveMQ's XBeanBrokerFactory builds the activemq.xml URL
            // from `-Dactivemq.conf=/c/.../conf`; without this rewrite the
            // broker start fails with "Системе не удается найти указанный
            // путь" (cannot find the specified path) at config load.
            #[cfg(windows)]
            let win_path = {
                let bytes = path.as_bytes();
                if bytes.len() >= 2
                    && bytes[0].is_ascii_alphabetic()
                    && (bytes[1] == b'/' || bytes[1] == b'\\')
                {
                    let mut s = String::with_capacity(path.len() + 1);
                    s.push(bytes[0] as char);
                    s.push(':');
                    s.push_str(&path[1..]);
                    Some(s)
                } else {
                    None
                }
            };
            #[cfg(not(windows))]
            let win_path: Option<String> = None;
            let try_paths: Vec<String> = match &win_path {
                Some(w) => vec![w.clone(), path.to_string(), rest.to_string()],
                None => vec![path.to_string(), rest.to_string()],
            };
            // Keep the FIRST (most-canonical) path's error: `try_paths` is
            // ordered [win_path?, path, rest], and the least-canonical `rest`
            // form (`/C:/...` on Windows) yields a spurious ERROR_INVALID_NAME
            // (os error 123) that would otherwise mask the real not-found error
            // from the canonical `C:/...` path.
            let mut first_err: Option<std::io::Error> = None;
            let mut result: Option<Vec<u8>> = None;
            for p in &try_paths {
                match std::fs::read(p) {
                    Ok(b) => {
                        result = Some(b);
                        break;
                    }
                    Err(e) => {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
            }
            match result {
                Some(b) => b,
                None => {
                    // A `file:` URL pointing at a DIRECTORY: HotSpot's
                    // `sun.net.www.protocol.file.FileURLConnection.getInputStream`
                    // does NOT fail — it returns a directory LISTING (the entry
                    // names, Collator-sorted, one per `\n`-terminated line) as a
                    // `ByteArrayInputStream`. `std::fs::read` on a directory
                    // errors instead (Windows: os error 5 "access denied"), so
                    // detect the directory and synthesize the same listing.
                    // (TestConfigFileLoader.test03: file: URL to
                    // test/webresources/dir1 expects a non-null stream.)
                    if let Some(d) = try_paths.iter().find(|p| std::path::Path::new(p).is_dir()) {
                        match std::fs::read_dir(d) {
                            Ok(entries) => {
                                let mut names: Vec<String> = entries
                                    .filter_map(|e| e.ok())
                                    .filter_map(|e| e.file_name().into_string().ok())
                                    .collect();
                                // HotSpot uses `Collator.getInstance()` (default
                                // locale, case-insensitive primary order). Mirror
                                // it closely with a case-insensitive sort, ties
                                // broken by the raw name for determinism.
                                names.sort_by(|a, b| {
                                    a.to_lowercase()
                                        .cmp(&b.to_lowercase())
                                        .then_with(|| a.cmp(b))
                                });
                                let mut listing = String::new();
                                for n in &names {
                                    listing.push_str(n);
                                    listing.push('\n');
                                }
                                listing.into_bytes()
                            }
                            Err(e) => return Err(fnfex(format!("{path} ({e})"))),
                        }
                    } else {
                        let e = first_err.unwrap_or_else(|| std::io::Error::other("no path tried"));
                        // The JDK's `file:` URL stream (sun.net.www.protocol.file
                        // FileURLConnection → FileInputStream) raises
                        // FileNotFoundException — not a bare IOException — when the
                        // target can't be opened (missing file or permission).
                        // Callers assert on it specifically, e.g. Spring Boot's
                        // PluginXmlParser via
                        // `withCauseInstanceOf(FileNotFoundException.class)`. See
                        // `apps/spring-boot/cratonvm-bug-reports/SB-12`.
                        return Err(fnfex(format!("{path} ({e})")));
                    }
                }
            }
        } else if let Some(name) = url_str.strip_prefix("classpath:") {
            let name = name.trim_start_matches('/');
            // Static app/boot/ext classpath first — preserves every case that
            // already worked (e.g. catalina.jar's mbeans-descriptors.xml).
            if let Some(b) = ctx.find_resource(name) {
                b
            } else if let Some(stream) = classpath_resource_via_context_loader(ctx, name) {
                // Resource lives behind the thread context classloader (a
                // deployed webapp's WEB-INF/classes), which `find_resource`
                // can't see. The real `ClasspathURLStreamHandler` resolves it
                // via `TCCL.getResource`; return the loader's own InputStream
                // directly. (TestPropertiesRoleMappingListener
                // testFileFromClasspath*: classpath:com/example/role-mapping.properties.)
                return Ok(Some(Value::Object(Some(stream))));
            } else {
                // Tomcat's real `ClasspathURLStreamHandler.openConnection`
                // throws `FileNotFoundException` (a subclass of IOException)
                // when neither the TCCL nor the handler's own loader resolves
                // the resource. Callers assert on that specific type — e.g.
                // `TestConfigFileLoader.test02` is `@Test(expected =
                // FileNotFoundException.class)` for `classpath:.../foo`. Mirror
                // the `file:` arm above (which already uses `fnfex`) so a
                // missing classpath resource raises FNFE, not a bare IOException.
                return Err(fnfex(format!(
                    "URL.openStream: classpath resource not found: {name}"
                )));
            }
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
            //
            // Spring's in-memory compiler deliberately uses the same
            // `resource:` spelling for URLs backed by an application-provided
            // URLStreamHandler. Prefer that handler when present: generated
            // annotation-processor outputs live only in its heap-resident
            // DynamicResourceFileObject and cannot be found through the VM's
            // static classpath resource index. Craton-synthesized resource
            // URLs have no handler and continue through the existing lookup.
            if let Some(conn) = url_custom_handler_connection(ctx, this)? {
                match conn {
                    Value::Object(Some(conn)) => {
                        let stream = ctx.invoke_virtual(
                            conn,
                            "getInputStream",
                            "()Ljava/io/InputStream;",
                            &[],
                        )?;
                        return Ok(Some(stream.unwrap_or(Value::Object(None))));
                    }
                    _ => return Ok(Some(Value::Object(None))),
                }
            }
            let name = name.trim_start_matches('/');
            ctx.find_resource(name)
                .ok_or_else(|| ioex(format!("URL.openStream: resource not found: {name}")))?
        } else if let Some(rest) = url_str.strip_prefix("jrt:") {
            // JEP 220 runtime-image URL: `jrt:/<module>/<resource>`. CratonVM
            // emits these from `find_all_resource_urls` when the boot classpath
            // is a jimage (`lib/modules`) rather than exploded `.jmod`s (HotSpot
            // always uses this form). `find_resource` keys on the module-relative
            // resource name, so strip the leading `/<module>/` before looking up.
            let path = rest.trim_start_matches('/');
            let resource = match path.split_once('/') {
                Some((_module, r)) => r,
                None => path,
            };
            ctx.find_resource(resource)
                .ok_or_else(|| ioex(format!("URL.openStream: jrt resource not found: {url_str}")))?
        } else if url_str.starts_with("http://") || url_str.starts_with("https://") {
            // GC-safe blocking region: the exchange blocks in native socket
            // reads, and the peer may be an in-process Java server (e.g.
            // MockWebServer in Spring's ResourceTests). Without parking this
            // thread, a concurrent STW request freezes the server's worker
            // threads while we sit in recv() — the 30s SO_RCVTIMEO then fires
            // as `Resource temporarily unavailable (os error 11)`
            // (ResourceTests.canCustomizeHttpUrlConnectionForRead). Pure OS
            // I/O, no ctx interaction inside → safe to park. Mirrors the
            // blocking-region use in http_url_connection.rs::perform.
            ctx.begin_blocking_region();
            let resp = http_perform_request("GET", &url_str, &[], &[], 10);
            ctx.end_blocking_region();
            let resp = resp.map_err(|e| ioex(format!("URL.openStream failed: {e}")))?;
            resp.body
        } else {
            // Application-provided `URLStreamHandler` (e.g. ShrinkWrap's
            // in-memory `archive:` handler). The scheme isn't one we resolve
            // natively, but the URL may carry a custom handler whose connection
            // reads from a heap-resident / custom source. Mirror the real
            // `URL.openStream()` = `openConnection().getInputStream()` by
            // delegating to it. A thrown `FileNotFoundException` propagates via
            // `?` (matching the JDK's missing-resource contract).
            match url_custom_handler_connection(ctx, this)? {
                Some(Value::Object(Some(conn))) => {
                    let stream =
                        ctx.invoke_virtual(conn, "getInputStream", "()Ljava/io/InputStream;", &[])?;
                    return Ok(Some(stream.unwrap_or(Value::Object(None))));
                }
                // Custom handler present but `openConnection` returned null:
                // surface a null stream rather than a bogus "unsupported scheme".
                Some(_) => return Ok(Some(Value::Object(None))),
                // No custom handler — genuinely unsupported.
                None => {}
            }
            return Err(ioex(format!(
                "URL.openStream: unsupported scheme: {url_str}"
            )));
        };

        if is_sf && spring_dbg_enabled() {
            eprintln!("[OSTR-DBG] URL.openStream bytes={}", bytes.len());
        }
        // Use the shared real-JDK-safe ByteArrayInputStream allocator rather
        // than writing presumed field slots. The latter made parser-style
        // single-byte reads observe an empty stream even though bulk reads
        // appeared to work, which broke XML resources inherited through a
        // filtered URLClassLoader.
        let stream = crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes);
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
            // Application-provided `URLStreamHandler` (e.g. ShrinkWrap
            // `archive:`): the real `URL.openConnection()` is
            // `handler.openConnection(this)`. Delegate so the app's own
            // `URLConnection` (which reads from its custom backing store) is
            // returned instead of the synthetic http/jar carrier below. Only
            // non-null, non-`sun.net.*` handlers reach this — `file:`/`jar:`/
            // synthetic URLs fall through to the carrier path unchanged.
            if let Some(conn @ Value::Object(Some(_))) = url_custom_handler_connection(ctx, this)? {
                return Ok(Some(conn));
            }
            // Determine the URL's external form so we can pick a carrier
            // class whose type matches what JDK callers cast the result to.
            // ActiveMQ's Main.getActiveMQHome() does
            //   `(JarURLConnection) url.openConnection()` on the
            //   `jar:file:…!/…` URL returned by ClassLoader.getResource() —
            // returning an HttpURLConnection there throws ClassCastException,
            // which is swallowed and forces a wrong `../.` home fallback.
            let ext = {
                let s5 = read_field_string_or(ctx, this, 5, "");
                // `s5.contains(':')` alone false-positives on a real-JDK
                // URL's field 5 (`authority`, e.g. "localhost:8080" — always
                // has a colon), skipping the toExternalForm() fallback below
                // and leaving `ext` as the bare authority. That misses both
                // the `jar:` and `http(s)://` prefix checks, so every real
                // http(s) URL fell to the generic URLConnection carrier and
                // `(HttpURLConnection) url.openConnection()` threw
                // ClassCastException everywhere (e.g. Tomcat's
                // TomcatBaseTest.methodUrl). Use the same synthetic-vs-real
                // discriminator as the openStream/toExternalForm paths above.
                let s = if field5_is_full_url(&s5) {
                    s5
                } else {
                    match ctx.invoke_virtual(this, "toExternalForm", "()Ljava/lang/String;", &[]) {
                        Ok(Some(Value::Object(Some(o)))) => ctx.read_string(o).unwrap_or_default(),
                        _ => {
                            let s0 = read_field_string_or(ctx, this, 0, "");
                            if s0.contains(':') {
                                s0
                            } else {
                                String::new()
                            }
                        }
                    }
                };
                s
            };
            // S111r23-DBG: log openConnection calls for spring.factories
            if ext.contains("spring.factories") && spring_dbg_enabled() {
                eprintln!("[CONN-DBG] URL.openConnection: {}", ext);
            }
            // For `jar:` URLs, retain the JarURLConnection carrier so callers
            // that cast it continue to work. All other schemes need the
            // concrete HttpURLConnection carrier, including `file:`. The
            // abstract URLConnection base has real-JDK bytecode for
            // getInputStream() which throws UnknownServiceException, and that
            // bytecode wins over a native registered on the base class. The
            // HttpURLConnection-specific native below instead dispatches to
            // URL.openStream() for every non-http scheme.
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
                if let Ok(Some(Value::Object(Some(o)))) =
                    ctx.invoke_virtual(url_obj, "toExternalForm", "()Ljava/lang/String;", &[])
                {
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
    // java/net/JarURLConnection.getJarFile() — return a `java/util/jar/JarFile`
    // opened on the enclosing JAR.
    //
    // `JarURLConnection` is ABSTRACT and `getJarFile()` is an abstract method
    // (no Code attribute). The concrete implementation lives in
    // `sun.net.www.protocol.jar.JarURLConnection` (or Spring Boot's own
    // nested-jar `URLConnection`). Our `URL.openConnection()` above hands out
    // a synthetic object whose *runtime* class is the abstract
    // `java/net/JarURLConnection`, so a virtual `getJarFile()` dispatch lands
    // on the abstract declaration and throws
    //   `AbstractMethodError: java/net/JarURLConnection.getJarFile() has no
    //    Code attribute`
    // which aborts Spring Boot 4's `JarLauncher` before it can read the fat
    // jar. Registering the native here completes the synthetic carrier the
    // same way `getJarFileURL()` / `getInputStream()` already do.
    //
    // The originating `jar:file:…!/entry` URL is stored in HUC_URL; strip the
    // `jar:file:` prefix and the `!/entry` suffix to recover the bare on-disk
    // jar path, then construct a `java/util/jar/JarFile` via its
    // `<init>(Ljava/lang/String;)V` native (`p59_jar_file_init`).
    r.register(
        "java/net/JarURLConnection",
        "getJarFile",
        "()Ljava/util/jar/JarFile;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let url_obj = match ctx.get_field(this, HUC_URL) {
                Value::Object(Some(o)) => o,
                _ => return Err(ioex("JarURLConnection.getJarFile: no URL")),
            };
            // Recover the full `jar:file:…!/…` external form.
            let mut ext = read_field_string_or(ctx, url_obj, 5, "");
            if !ext.starts_with("jar:") {
                if let Ok(Some(Value::Object(Some(o)))) =
                    ctx.invoke_virtual(url_obj, "toExternalForm", "()Ljava/lang/String;", &[])
                {
                    ext = ctx.read_string(o).unwrap_or_default();
                }
            }
            // jar:file:<path>!/<entry>  →  <path>  (the outer jar on disk).
            // For a double-nested Spring Boot 2.x URL
            //   jar:file:/fat.jar!/BOOT-INF/lib/inner.jar!/entry
            // the *first* `!/`-delimited segment is still the outer jar.
            let after_scheme = ext
                .strip_prefix("jar:file:")
                .or_else(|| ext.strip_prefix("jar:"))
                .unwrap_or(&ext);
            let jar_part = after_scheme
                .split("!/")
                .next()
                .unwrap_or("")
                .trim_start_matches("file:")
                .to_string();
            if jar_part.is_empty() {
                return Err(ioex("JarURLConnection.getJarFile: malformed URL"));
            }
            // Resolve to a real on-disk path. `file:` URLs use a leading `/`
            // before a Windows drive letter (`/C:/…`); try the trimmed form
            // first, then the raw form for POSIX absolute paths — mirrors the
            // `URL.openStream` resolution above.
            let trimmed = jar_part.trim_start_matches('/');
            let disk_path = if std::path::Path::new(trimmed).exists() {
                trimmed.to_string()
            } else if std::path::Path::new(&jar_part).exists() {
                jar_part.clone()
            } else {
                // The enclosing jar genuinely does not exist on disk. The real
                // `JarURLConnection.getJarFile()` throws `FileNotFoundException`
                // here; our `JarFile.<init>` did NOT, so the connection handed
                // back an empty JarFile and Spring's
                // `AbstractFileResolvingResource.exists()` (which treats
                // `entryName == null` — i.e. a `…!/` jar-root URL — as existing
                // once `getJarFile()` returns) reported a NON-EXISTENT jar as
                // present (PMRPR `javaDashJarFinds…` `writeAssetJar`
                // `jar:file:X<path>!/` assertion). Surface the missing jar so
                // the `catch (IOException)` turns it into `false`.
                return Err(fnfex(format!(
                    "JarURLConnection.getJarFile: no such jar file: {jar_part}"
                )));
            };
            // Allocate the JarFile and run its <init>(String) native so the
            // `path` (field 0) and parsed `manifest` (field 1) are populated.
            let jar_file = match ctx.new_object("java/util/jar/JarFile")? {
                Some(Value::Object(Some(o))) => o,
                _ => return Err(ioex("JarURLConnection.getJarFile: alloc JarFile")),
            };
            let path_str = ctx.create_string(&disk_path);
            ctx.invoke_special(
                "java/util/jar/JarFile",
                "<init>",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(jar_file)), Value::Object(Some(path_str))],
            )?;
            Ok(Some(Value::Object(Some(jar_file))))
        },
    );
    // java/net/JarURLConnection.getContentLengthLong() / getContentLength() —
    // return the jar ENTRY's uncompressed size. Our synthetic carrier has no
    // JDK URLConnection header machinery, so the inherited URLConnection
    // implementation returns -1. Spring's
    // `AbstractFileResolvingResource.isReadable()` treats `contentLength <= 0`
    // (with no exception) as NOT readable, so a perfectly good jar class
    // resource reported `isReadable=false`, `contentLength=-1`, and
    // `classpath*:…/*.class` scanning matched 0 resources (SB-07 / S10_Resources).
    // Recover the entry from the `jar:file:…!/entry` URL in HUC_URL and read
    // its size straight from the zip central directory. The lookup walks the
    // superclass chain starting at the receiver's JarURLConnection class, so it
    // wins over the inherited URLConnection/HttpURLConnection registrations.
    r.register(
        "java/net/JarURLConnection",
        "getContentLengthLong",
        "()J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Long(jar_url_conn_entry_size(ctx, this))))
        },
    );
    r.register(
        "java/net/JarURLConnection",
        "getContentLength",
        "()I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sz = jar_url_conn_entry_size(ctx, this);
            // URLConnection.getContentLength() narrows to int, -1 when unknown
            // or larger than Integer.MAX_VALUE.
            let v = if sz < 0 || sz > i32::MAX as i64 {
                -1
            } else {
                sz as i32
            };
            Ok(Some(Value::Int(v)))
        },
    );
    // java/net/JarURLConnection.getJarEntry() — the no-arg accessor that
    // Spring's checkReadable() and PathMatchingResourcePatternResolver use to
    // probe a jar resource. Our synthetic carrier's runtime class is the
    // ABSTRACT java/net/JarURLConnection, so the virtual dispatch would land on
    // the abstract declaration (AbstractMethodError) without this native.
    r.register(
        "java/net/JarURLConnection",
        "getJarEntry",
        "()Ljava/util/jar/JarEntry;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ext = jar_url_conn_ext(ctx, this);
            Ok(Some(jar_url_lookup_entry(ctx, &ext)))
        },
    );
    // java/net/JarURLConnection.getEntryName() — the entry path inside the jar
    // (the part after `!/`), or null for a bare `jar:file:…!/` (jar root).
    //
    // Without this native the inherited real-JDK accessor reads the synthetic
    // carrier's never-populated `entryName` field and returns null even for
    // `jar:file:…!/some/entry`. Spring's `AbstractFileResolvingResource.exists()`
    // does `entryName == null || getJarEntry() != null`, so a null entryName
    // made a NON-EXISTENT jar entry report as existing (PMRPR
    // `javaDashJarFinds…` `writeAssetJar` asserting `…!/assets/none.txt` is
    // absent). Parse it the same way `jar_url_lookup_entry` does so the two stay
    // consistent.
    r.register(
        "java/net/JarURLConnection",
        "getEntryName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ext = jar_url_conn_ext(ctx, this);
            let entry = ext
                .strip_prefix("jar:file:")
                .or_else(|| ext.strip_prefix("jar:"))
                .and_then(|a| a.splitn(2, "!/").nth(1))
                .filter(|e| !e.is_empty());
            match entry {
                Some(e) => {
                    let s = ctx.create_string(e);
                    Ok(Some(Value::Object(Some(s))))
                }
                None => Ok(Some(Value::Object(None))),
            }
        },
    );

    // URLConnection.setUseCaches / setDefaultUseCaches / connect — Spring's
    // `ResourceUtils.useCachesIfNecessary` calls setUseCaches(false) on
    // file: URLs; without these no-op natives the call would fall through
    // to the real-JDK setter, which probes the (uninitialised) connected
    // field and throws IllegalStateException. Make them no-ops on both
    // URLConnection and HttpURLConnection (registered separately).
    r.register(
        "java/net/URLConnection",
        "setUseCaches",
        "(Z)V",
        |_ctx, _args| Ok(None),
    );
    r.register(
        "java/net/URLConnection",
        "setDefaultUseCaches",
        "(Z)V",
        |_ctx, _args| Ok(None),
    );
    r.register("java/net/URLConnection", "connect", "()V", |_ctx, _args| {
        Ok(None)
    });
    r.register(
        "java/net/URLConnection",
        "getContentLength",
        "()I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let url = huc_url_string(ctx, this);
            let len = synthetic_resource_url_content_len(ctx, &url);
            let v = if len < 0 || len > i32::MAX as i64 {
                -1
            } else {
                len as i32
            };
            Ok(Some(Value::Int(v)))
        },
    );
    r.register(
        "java/net/URLConnection",
        "getContentLengthLong",
        "()J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let url = huc_url_string(ctx, this);
            Ok(Some(Value::Long(synthetic_resource_url_content_len(
                ctx, &url,
            ))))
        },
    );
    r.register(
        "java/net/URLConnection",
        "getLastModified",
        "()J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let url = huc_url_string(ctx, this);
            Ok(Some(Value::Long(synthetic_resource_url_last_modified(
                &url,
            ))))
        },
    );
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
            ctx.invoke_virtual(url_obj, "openStream", "()Ljava/io/InputStream;", &[])
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
    // (REMOVED) Spring `ConfigurationClassEnhancer.enhance(Class, ClassLoader)`
    // identity-bypass shim.
    //
    // FIX(net-phase-e #2): this used to register an identity-bypass that
    // returned the @Configuration class unchanged, disabling CGLIB @Bean
    // interception (a silent-wrong-result stub: inter-@Bean-method calls
    // returned fresh instances instead of the shared singleton).
    //
    // The REAL CGLIB @Configuration enhancement is now implemented in
    // `cglib_enhancer.rs` (`cce_enhance` / `register_cglib_enhancer`), which
    // emits a genuine EnhancedConfiguration subclass and intercepts @Bean
    // methods. That registration runs in `lib.rs::register_net_natives`
    // AFTER `net_phase_e::register_phase_e_networking` and re-registers the
    // identical method triple
    //   org/springframework/context/annotation/ConfigurationClassEnhancer
    //   .enhance(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/lang/Class;
    // Registry semantics silently overwrite on duplicate triples
    // (native-api registry), so this bypass was already DEAD CODE —
    // unconditionally clobbered by the real enhancer. Removing it eliminates
    // the misleading stub; the live behavior is unchanged (real path wins).
    // Do not re-add a bypass here: if the real enhancer needs work, fix it in
    // `cglib_enhancer.rs`.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // (REMOVED) AbstractBeanDefinition.getResolvedAutowireMode() override.
    //
    // A former "Bug 3" workaround hardcoded getResolvedAutowireMode() to
    // return 0 (AUTOWIRE_NO) for AbstractBeanDefinition + Root/Generic/Child
    // subclasses, to mask a field-layout/slot bug where the `autowireMode`
    // field misread as a spurious non-zero value (driving an unwanted
    // autowireByType setter pass that crashed ConfigurationClassPostProcessor).
    //
    // That underlying field bug is FIXED: getAutowireMode() (which reads the
    // same `autowireMode` slot) now returns the real value (0/2/3) identically
    // to HotSpot for default / setAutowireMode(BY_TYPE) / setAutowireMode(
    // CONSTRUCTOR) bean definitions. The constant-0 stub had become stale and
    // actively wrong: it forced AUTOWIRE_NO onto @Bean factory methods, whose
    // definitions are AUTOWIRE_CONSTRUCTOR (3). That sent
    // ConstructorResolver.createArgumentArray down the `autowiring == false`
    // branch, so a @Qualifier'd @Bean method parameter could not be autowired
    // and Spring threw "Ambiguous argument values for parameter of type ..."
    // (SB-05 / S02_Inject). Removing the stub restores the real bytecode:
    // getResolvedAutowireMode() reads the (correct) field and returns it,
    // matching HotSpot. Daemon apps using the default AUTOWIRE_NO still get 0.
    // -----------------------------------------------------------------------

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
    // injection on a @Configuration class — the CGLIB enhancement is now
    // performed by the real enhancer in `cglib_enhancer.rs`; the former
    // identity-bypass shim here was removed, see the "(REMOVED)
    // ConfigurationClassEnhancer.enhance" note above).  The
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
        |_ctx, _args| Ok(None),
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
            Ok(None)
        },
    );

    // Same chain: RedisHttpSessionConfiguration.redisMessageListenerContainer()
    // can call this setter with a null factory while its broken multi-argument
    // autowiring path is being bypassed.  Keep that narrow bootstrap escape,
    // but execute the real setter for a valid factory.  The former unconditional
    // no-op swallowed legitimate injection in Spring Data Redis's own
    // DataRedisAnnotationDrivenConfiguration tests, leaving the container
    // unusable at afterPropertiesSet().
    r.register(
        "org/springframework/data/redis/listener/RedisMessageListenerContainer",
        "setConnectionFactory",
        "(Lorg/springframework/data/redis/connection/RedisConnectionFactory;)V",
        |ctx, args| match args.get(1) {
            Some(Value::Object(Some(_))) => ctx.invoke_special_bytecode_only(
                "org/springframework/data/redis/listener/RedisMessageListenerContainer",
                "setConnectionFactory",
                "(Lorg/springframework/data/redis/connection/RedisConnectionFactory;)V",
                args,
            ),
            _ => Ok(None),
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
        |_ctx, _args| Ok(None),
    );

    // -----------------------------------------------------------------------
    // Round 76 (REVERTED): do NOT stub ScheduledTaskRegistrar.schedule*Task.
    //
    // A previous workaround registered scheduleCronTask/scheduleFixedRateTask/
    // scheduleFixedDelayTask/scheduleTriggerTask as no-ops returning null to
    // dodge a "No scheduled future" assertion in one Redis-session app whose
    // cron fired synchronously through the DelegatedScheduledExecutorService
    // path.  That stub is far too broad: shadowing these methods bypasses the
    // real bytecode, so the registrar's cronTasks/fixedRateTasks/
    // fixedDelayTasks/triggerTasks lists are never populated (they are filled
    // by the addCronTask()/addFixedRateTask()/... calls in the taskScheduler==
    // null branch), and getScheduledTasks() collects a null ScheduledTask.
    // This breaks all @Scheduled processing — ScheduledAnnotationBeanPostProcessor
    // tests saw null task lists and NPEs (scheduleOneTimeTask, which was never
    // stubbed, was the only path that worked).  The real fix for the Redis app
    // belongs in the ConcurrentTaskScheduler/ReschedulingRunnable execution
    // path, not here; letting the real methods run matches HotSpot.

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
            let url_val = ctx.invoke_virtual(this, "getURL", "()Ljava/net/URL;", &[])?;
            let url_obj = match url_val {
                Some(Value::Object(Some(o))) => o,
                _ => return Err(ioex("UrlResource.getInputStream: getURL() returned null")),
            };
            // http(s) URLs: mirror Spring's REAL bytecode
            //   openConnection() → customizeConnection(con) → con.getInputStream()
            // instead of short-circuiting to URL.openStream(). Two reasons
            // (ResourceTests):
            //   * `customizeConnection(HttpURLConnection)` subclass overrides
            //     stage request headers (canCustomizeHttpUrlConnectionForRead
            //     asserts its Framework-Name header reaches the server) — the
            //     openStream shortcut never ran them;
            //   * the connection path performs the exchange inside GC-safe
            //     blocking regions (http_url_connection.rs::perform), so an
            //     in-process MockWebServer keeps serving during the read
            //     (openStream's exchange previously EAGAIN-timed-out under a
            //     concurrent STW).
            // `this`/`url_obj`/`con` are pinned across the up-calls below:
            // toExternalForm/openConnection/customizeConnection run arbitrary
            // Java (allocations can move objects).
            let this_pin = ctx.pin_native_root(this);
            let url_pin = ctx.pin_native_root(url_obj);
            let ext_val = ctx.invoke(
                "java/net/URL",
                "toExternalForm",
                "()Ljava/lang/String;",
                &[Value::Object(Some(url_obj))],
            );
            let this = ctx.read_native_pin(this_pin, this);
            let url_obj = ctx.read_native_pin(url_pin, url_obj);
            let ext = match ext_val {
                Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            if ext.starts_with("http://") || ext.starts_with("https://") {
                let con_val = ctx.invoke_virtual(
                    url_obj,
                    "openConnection",
                    "()Ljava/net/URLConnection;",
                    &[],
                );
                let this = ctx.read_native_pin(this_pin, this);
                let con = match con_val {
                    Ok(Some(Value::Object(Some(c)))) => c,
                    Err(e) => {
                        ctx.unpin_native_roots(this_pin);
                        return Err(e);
                    }
                    _ => {
                        ctx.unpin_native_roots(this_pin);
                        return Err(ioex(
                            "UrlResource.getInputStream: openConnection returned null",
                        ));
                    }
                };
                let con_pin = ctx.pin_native_root(con);
                // Protected on AbstractFileResolvingResource; virtual dispatch
                // reaches subclass overrides (its real bytecode no-ops the
                // useCaches hook and forwards to the HttpURLConnection overload).
                let cc = ctx.invoke_virtual(
                    this,
                    "customizeConnection",
                    "(Ljava/net/URLConnection;)V",
                    &[Value::Object(Some(con))],
                );
                let con = ctx.read_native_pin(con_pin, con);
                if let Err(e) = cc {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
                // Perform the request (headers now staged) and inspect the
                // status BEFORE handing out the body stream. Two cases must
                // keep the legacy openStream behaviour the connection path
                // lacks:
                //   * 3xx — the connection path does not follow redirects,
                //     while URL.openStream (and the real JDK's default
                //     followRedirects) does;
                //   * -1 — huc_real_perform folds connect failures into -1
                //     per the getResponseCode contract, which must not become
                //     a silent EMPTY stream here; openStream re-attempts and
                //     raises the real IOException.
                // A SocketTimeoutException from getResponseCode propagates
                // as-is (no fallback), matching the real JDK.
                let code_val = ctx.invoke_virtual(con, "getResponseCode", "()I", &[]);
                let con = ctx.read_native_pin(con_pin, con);
                let url_obj = ctx.read_native_pin(url_pin, url_obj);
                ctx.unpin_native_roots(this_pin);
                let code = match code_val {
                    Ok(Some(Value::Int(c))) => c,
                    Err(e) => return Err(e),
                    _ => -1,
                };
                if code == -1 || matches!(code, 301 | 302 | 303 | 307 | 308) {
                    return ctx.invoke_virtual(
                        url_obj,
                        "openStream",
                        "()Ljava/io/InputStream;",
                        &[],
                    );
                }
                return ctx.invoke_virtual(con, "getInputStream", "()Ljava/io/InputStream;", &[]);
            }
            ctx.unpin_native_roots(this_pin);
            // Everything else: delegate directly to URL.openStream() — our
            // native handles jar:file: double-nested URLs correctly.
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
    r.register(
        huc,
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // S111r22 — For non-HTTP/HTTPS URLs (jar:file:, file:, classpath:, jrt:),
            // huc_perform would try to make a real HTTP request which fails. Instead,
            // delegate to URL.openStream() via the stored HUC_URL. This enables
            // SpringFactoriesLoader to read META-INF/spring.factories from nested JARs
            // when it goes through url.openConnection().getInputStream().
            let url_str = huc_url_string(ctx, this);
            if !url_str.starts_with("http://") && !url_str.starts_with("https://") {
                // Non-HTTP: delegate to URL.openStream() on the stored URL object.
                let url_obj = match ctx.get_field(this, HUC_URL) {
                    Value::Object(Some(o)) => o,
                    _ => {
                        return Err(ioex(
                            "HttpURLConnection.getInputStream: no URL for non-http",
                        ))
                    }
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
            ctx.set_field(stream, 1, Value::Int(0)); // pos
            ctx.set_field(stream, 2, Value::Int(0)); // mark
            ctx.set_field(stream, 3, Value::Int(len)); // count
            Ok(Some(Value::Object(Some(stream))))
        },
    );
    // The generic carrier for `file:` and other non-HTTP URLs is a synthetic
    // HttpURLConnection so its getInputStream native wins over URLConnection's
    // real-JDK default body. Its inherited metadata accessors would otherwise
    // read uninitialised URLConnection fields and report zero. Preserve the
    // actual file timestamp for both direct getLastModified() callers and the
    // getHeaderFieldDate("last-modified", ...) shape used by Spring Boot's
    // JarUrlConnectionTests and NestedUrlConnectionTests.
    r.register(huc, "getLastModified", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let url = huc_origin_url_string(ctx, this);
        let value = if url.starts_with("http://") || url.starts_with("https://") {
            0
        } else {
            synthetic_resource_url_last_modified(&url)
        };
        Ok(Some(Value::Long(value)))
    });
    r.register(
        huc,
        "getHeaderFieldDate",
        "(Ljava/lang/String;J)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name =
                value_or_string(ctx, args.get(1).copied().unwrap_or(Value::Object(None)), "");
            let fallback = match args.get(2).copied() {
                Some(Value::Long(value)) => value,
                _ => 0,
            };
            let url = huc_origin_url_string(ctx, this);
            if !url.starts_with("http://")
                && !url.starts_with("https://")
                && name.eq_ignore_ascii_case("last-modified")
            {
                let modified = synthetic_resource_url_last_modified(&url);
                // RFC 1123 dates carry whole-second precision. FileURLConnection
                // therefore rounds the filesystem's millisecond timestamp down
                // before exposing it as the `last-modified` header date.
                let header_date = modified / 1_000 * 1_000;
                return Ok(Some(Value::Long(if header_date == 0 {
                    fallback
                } else {
                    header_date
                })));
            }
            Ok(Some(Value::Long(fallback)))
        },
    );
    r.register(
        huc,
        "getHeaderField",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
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
        },
    );
    r.register(
        huc,
        "getHeaderField",
        "(I)Ljava/lang/String;",
        |ctx, args| {
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
        },
    );
    r.register(huc, "getContentLength", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let url = huc_url_string(ctx, this);
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Ok(Some(Value::Int(-1)));
        }
        huc_perform(ctx, this)?;
        match ctx.get_field(this, HUC_BODY) {
            Value::Object(Some(a)) => Ok(Some(Value::Int(ctx.array_length(a) as i32))),
            _ => Ok(Some(Value::Int(-1))),
        }
    });
    r.register(
        huc,
        "setRequestMethod",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mval = args.get(1).copied().unwrap_or(Value::Object(None));
            let method = value_or_string(ctx, mval, "GET");
            let s = ctx.create_string(&method);
            ctx.set_field(this, HUC_METHOD, Value::Object(Some(s)));
            Ok(None)
        },
    );
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
        // Real-JDK carrier (slot 0 holds the real `URLConnection.url` object,
        // not our synthetic conn-id int): set the REAL `doOutput` field by name
        // so the inherited `getDoOutput()` bytecode reports the caller's value.
        // Writing the synthetic `HUC_DO_OUTPUT` slot on a real object aliases
        // `connected` — it leaves `doOutput` false (Spring then skips the
        // request-body write → server blocks on the promised Content-Length →
        // "Read timed out") AND spuriously marks the connection connected.
        if matches!(ctx.get_field(this, 0), Value::Object(Some(_))) {
            ctx.set_field_by_name(this, "doOutput", Value::Int(v));
        } else {
            ctx.set_field(this, HUC_DO_OUTPUT, Value::Int(v));
        }
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
        |_ctx, _args| Ok(None),
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
            let is_jar = matches!(proto.as_str(), "jar" | "war" | "zip" | "vfszip" | "wsjar");
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
    // missing subsystems (JSF) and destabilize bootstrap.
    //
    // Groovy note (2026-07-07): this stub used to also force `groovy.*` to
    // absent, because letting Spring's `DelegatingSmartContextLoader` pick the
    // Groovy context loader used to wedge the VM in Groovy's shaded ANTLR4
    // compiler (millions of gc::guard OOB-field warnings, no completion). The
    // real culprit was OUR force-registered ANTLR ATN fast-path shim
    // (`native_antlr_atn_config_init` & friends in lib.rs, added e48e14cc)
    // applying standard-ANTLR4 `ATNConfig` slot indices (reachesIntoOuterContext=3,
    // semanticContext=4) to the `groovyjarjarantlr4` shaded copy — which is the
    // tunnelvisionlabs fork whose base `ATNConfig` legitimately has only THREE
    // fields (state / packed altAndOuterContextDepth / context) with subclass
    // fields at slots 3..5. Fixed by 50119adb (fork-aware packed layout +
    // `antlr_groovy_atn_special_slot`), so `groovy.*` presence is now decided
    // honestly by Spring's bytecode implementation below and `.groovy` bean scripts load
    // through the real `GenericGroovyXmlContextLoader`. See
    // docs/known-issues/test-context-constructor-param-annotation-offset.md.
    r.register(
        "org/springframework/util/ClassUtils",
        "isPresent",
        "(Ljava/lang/String;Ljava/lang/ClassLoader;)Z",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            if name == "jakarta.faces.context.FacesContext" {
                return Ok(Some(Value::Int(0)));
            }
            // Preserve Spring's loader-specific semantics, including its
            // canonical-inner-name fallback (`Outer.Inner` -> `Outer$Inner`).
            // A global native lookup incorrectly reported such present classes
            // absent during auto-configuration exclusion validation.
            ctx.invoke_special_bytecode_only(
                "org/springframework/util/ClassUtils",
                "isPresent",
                "(Ljava/lang/String;Ljava/lang/ClassLoader;)Z",
                args,
            )
        },
    );

    // S111r26 — JSF integration is optional; when the Faces API is absent,
    // Spring's FacesDependencyRegistrar probe should effectively no-op.
    // In our current runtime, that probe can escalate into hard failure via
    // NoClassDefFoundError on jakarta.faces.*. Short-circuit it.
    //
    // NOTE: the blanket `registerWebApplicationScopes` no-ops that used to sit
    // here are gone (context.annotation cluster fix). They pre-dated S111r27's
    // more surgical `ClassUtils.isPresent` fix below, which makes the real
    // `WebApplicationContextUtils.jsfPresent` static flag correctly compute
    // `false` for "jakarta.faces.context.FacesContext" — so the real
    // `registerWebApplicationScopes` body already skips
    // `FacesDependencyRegistrar` on its own and safely runs
    // `beanFactory.registerScope(SCOPE_REQUEST/SCOPE_SESSION, ...)`. Shadowing
    // it here was dropping those `registerScope` calls entirely, breaking
    // every plain (non-Boot) `GenericWebApplicationContext.refresh()` with
    // "IllegalStateException: No Scope registered for scope name 'request'/
    // 'session'" (ClassPathBeanDefinitionScannerJsr330ScopeIntegrationTests,
    // ClassPathBeanDefinitionScannerScopeIntegrationTests). Only the
    // `FacesDependencyRegistrar` probe itself stays stubbed below, as a
    // defense-in-depth backstop (it should now be unreachable).
    r.register(
        "org/springframework/web/context/support/WebApplicationContextUtils$FacesDependencyRegistrar",
        "registerFacesDependencies",
        "(Lorg/springframework/beans/factory/config/ConfigurableListableBeanFactory;)V",
        |_ctx, _args| Ok(None),
    );

    // S111r57 — dispatch to Spring Boot's real generic
    // ServletWebServerApplicationContext.getWebServerFactory() implementation.
    // The generic context must select its own registered factory backend.
    //
    // In real Spring Boot, this protected method calls
    //   getBeanFactory().getBeanNamesForType(ServletWebServerFactory.class)
    // and throws MissingWebServerFactoryBeanException if zero matches. Under
    // CratonVM the auto-configuration that registers the Tomcat factory bean
    // never completes (Cglib/condition-evaluation issues upstream), so the
    // lookup fails. The old native short-circuit constructed a Tomcat factory
    // unconditionally, which is invalid for Jetty, Undertow, and generic modules:
    // they legitimately do not have Spring Boot's Tomcat implementation on their
    // class path. Execute the original bytecode body instead, which either finds
    // the registered backend or throws Spring's normal Java exception.
    //
    // SB 2.x: context = org/springframework/boot/web/servlet/context/ServletWebServerApplicationContext
    //         factory = org/springframework/boot/web/servlet/server/ServletWebServerFactory
    //         impl    = org/springframework/boot/web/embedded/tomcat/TomcatServletWebServerFactory
    //
    // SB 4.x: context = org/springframework/boot/web/server/servlet/context/ServletWebServerApplicationContext
    //         factory = org/springframework/boot/web/server/servlet/ServletWebServerFactory
    //         impl    = org/springframework/boot/tomcat/servlet/TomcatServletWebServerFactory

    // SB 4.x
    r.register(
        "org/springframework/boot/web/server/servlet/context/ServletWebServerApplicationContext",
        "getWebServerFactory",
        "()Lorg/springframework/boot/web/server/servlet/ServletWebServerFactory;",
        |ctx, args| {
            ctx.invoke_special_bytecode_only(
                "org/springframework/boot/web/server/servlet/context/ServletWebServerApplicationContext",
                "getWebServerFactory",
                "()Lorg/springframework/boot/web/server/servlet/ServletWebServerFactory;",
                args,
            )
        },
    );

    // SB 2.x
    r.register(
        "org/springframework/boot/web/servlet/context/ServletWebServerApplicationContext",
        "getWebServerFactory",
        "()Lorg/springframework/boot/web/servlet/server/ServletWebServerFactory;",
        |ctx, args| {
            ctx.invoke_special_bytecode_only(
                "org/springframework/boot/web/servlet/context/ServletWebServerApplicationContext",
                "getWebServerFactory",
                "()Lorg/springframework/boot/web/servlet/server/ServletWebServerFactory;",
                args,
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
    fn alloc_netty_reactive_factory(ctx: &mut dyn NativeContext) -> MethodCallResult {
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

    // residual-4 fix: the two `AbstractFileResolvingResource.customizeConnection`
    // no-ops above were REMOVED. They made `customizeConnection` a complete
    // no-op for every caller — including `AbstractFileResolvingResource.exists()`/
    // `contentLength()`/`lastModified()`, which call it directly (not just via
    // the separately-intercepted `UrlResource.getInputStream()` above) — so a
    // subclass overriding `customizeConnection(HttpURLConnection)` (e.g. to set
    // a custom request header) was silently never invoked, and
    // `useCachesIfNecessary`+the `instanceof HttpURLConnection` dispatch never
    // ran either. The original S111r24 motivation (`getClass().getSimpleName()`
    // throwing on a synthetic HttpURLConnection Class mirror, breaking
    // `UrlResource.getInputStream()`'s uncaught-exception path) is already
    // covered independently by the `UrlResource.getInputStream()` override
    // above, which bypasses `customizeConnection` entirely — and empirically,
    // `getClass().getSimpleName()` on the connection objects `customizeConnection`
    // actually receives now works fine (verified: real `sun.net.www.protocol.
    // http.HttpURLConnection`/`HttpsURLConnectionImpl` instances, not a synthetic
    // mirror). Letting the real bytecode run restores the real JDK contract:
    // `useCachesIfNecessary` + the `instanceof`-gated dispatch to
    // `customizeConnection(HttpURLConnection)`, which virtual-dispatches to
    // whatever subclass override exists (`UrlResource`'s own subclasses use
    // this to set custom request headers before `exists()`/`getInputStream()`
    // send the request — `ResourceTests.UrlResourceTests
    // .canCustomizeHttpUrlConnectionForExists[Fallback]`).
}

// ===========================================================================
// RE.5 — java.net.http.HttpClient
// ===========================================================================

// Synthetic `java/net/http/HttpResponse` field layout used by the bare
// real-JDK `HttpClient` model. `HttpClient`/`HttpResponse` are abstract JDK
// types, so `newHttpClient()`/`send()` hand back synthetic instances whose
// runtime class IS the abstract class — every instance method therefore has to
// be registered directly on it (a real concrete `HttpClientImpl`/
// `HttpResponseImpl` is never materialised on this path).
const RE5_RESP_STATUS: usize = 0; // Int
const RE5_RESP_BODY_BYTES: usize = 1; // byte[]
const RE5_RESP_HEADERS: usize = 2; // String[] of "key: value"
const RE5_RESP_HANDLER_TAG: usize = 3; // String: how body() materialises
const RE5_RESP_BODY_OBJ: usize = 4; // Object: the RE5_TAG_HANDLED body value
const RE5_RESP_NUM_FIELDS: usize = 5;

/// `RE5_RESP_HANDLER_TAG` value meaning "the body was produced by driving a
/// real user-supplied BodyHandler; `body()` returns `RE5_RESP_BODY_OBJ`".
const RE5_TAG_HANDLED: &str = "handled";

// Synthetic `java/net/http/HttpResponse$ResponseInfo` handed to a real
// BodyHandler's `apply` (see `re5_drive_body_handler`).
const RE5_RESPONSE_INFO: &str = "java/net/http/HttpResponse$ResponseInfo";
const RE5_RI_STATUS: usize = 0; // Int
const RE5_RI_HEADERS: usize = 1; // String[] of "key: value"
const RE5_RI_NUM_FIELDS: usize = 2;

// One-shot replay `Flow.Subscription` handed to a real BodySubscriber (see
// `re5_drive_body_handler`). Registered on its own wrapper class name -- NOT
// on `java/util/concurrent/Flow$Subscription` itself, whose `request`/
// `cancel` natives are already owned by the synthetic-Flow demand counters
// in `streams.rs`/`phases_late.rs` with an incompatible field layout.
// Abstract interface dispatch finds receiver-class natives (the
// `Enumeration$Impl` wrapper pattern, vm_exec C25).
const RE5_REPLAY_SUBSCRIPTION: &str = "cratonvm/net/HttpBodyReplaySubscription";
const RE5_SUB_SUBSCRIBER: usize = 0; // Flow.Subscriber the body is delivered to
const RE5_SUB_BODY: usize = 1; // byte[] wire body (null = empty body)
const RE5_SUB_STATE: usize = 2; // Int: 0 = pending, 1 = delivered, 2 = cancelled
const RE5_SUB_NUM_FIELDS: usize = 3;

/// Read a `byte[]` heap object into a `Vec<u8>` (high byte ignored).
fn re5_read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    re5_read_byte_array_range(ctx, arr, 0, ctx.array_length(arr))
}

/// Read a slice of a `byte[]` heap object into a `Vec<u8>`.
fn re5_read_byte_array_range(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    off: usize,
    len: usize,
) -> Vec<u8> {
    let cap = ctx.array_length(arr);
    let start = off.min(cap);
    let end = start.saturating_add(len).min(cap);
    let mut out = Vec::with_capacity(end.saturating_sub(start));
    for i in start..end {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            out.push((b & 0xff) as u8);
        }
    }
    out
}

/// Classify the BodyHandler passed to `send`/`sendAsync`.
///
/// Our synthetic `BodyHandlers` factories stash a tag string in slot 0
/// ("string" / "inputstream" / "bytearray" / "discarding") — those (and a
/// missing/null handler, which degrades to an InputStream body) return
/// `Some(tag)`, and `HttpResponse.body()` materialises the raw wire bytes
/// per the tag.
///
/// Any OTHER object is a real user-supplied `BodyHandler` implementation —
/// e.g. Spring's `JdkClientHttpRequest$DecompressingBodyHandler` (wraps
/// `ofInputStream` in a GZIP/Inflater stream) or the `BodyHandlers
/// .ofPublisher()` lambda (reactive `JdkClientHttpConnector`) — and returns
/// `None`: the caller must drive the real BodyHandler protocol via
/// [`re5_drive_body_handler`]. The pre-fix version silently defaulted these
/// to a raw InputStream, skipping the user's body transformation entirely
/// (gzip/deflate response bodies reached Spring still compressed —
/// `JdkClientHttpRequestFactoryTests.compressionGzip/compressionDeflate`).
fn re5_handler_tag(ctx: &dyn NativeContext, handler: Option<Value>) -> Option<String> {
    if let Some(Value::Object(Some(h))) = handler {
        let cid = ctx.class_id_of_object(h);
        if ctx.class_name_of_id(cid).as_deref() == Some("java/net/http/HttpResponse$BodyHandler") {
            if let Value::Object(Some(s)) = ctx.get_field(h, 0) {
                if let Some(tag) = ctx.read_string(s) {
                    return Some(tag);
                }
            }
            // Synthetic-but-tagless: keep the legacy InputStream default
            // (a bare synthetic handler has no real apply() to drive).
            return Some("inputstream".to_string());
        }
        return None;
    }
    Some("inputstream".to_string())
}

/// Build the synthetic `java/net/http/HttpResponse` carrying the wire result.
fn re5_build_response(
    ctx: &mut dyn NativeContext,
    status: i32,
    headers: &[(String, String)],
    body: &[u8],
    handler_tag: &str,
) -> ObjectRef {
    let out = alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse", RE5_RESP_NUM_FIELDS);
    ctx.set_field(out, RE5_RESP_STATUS, Value::Int(status));
    let body_arr = ctx.new_array(ArrayElementType::Byte, body.len());
    for (i, b) in body.iter().enumerate() {
        ctx.set_array_element(body_arr, i, Value::Int(*b as i32));
    }
    ctx.set_field(out, RE5_RESP_BODY_BYTES, Value::Object(Some(body_arr)));
    let hdr_arr = ctx.new_ref_array(ClassId::new(0), headers.len());
    for (i, (k, v)) in headers.iter().enumerate() {
        let s = ctx.create_string(&format!("{k}: {v}"));
        ctx.set_array_element(hdr_arr, i, Value::Object(Some(s)));
    }
    ctx.set_field(out, RE5_RESP_HEADERS, Value::Object(Some(hdr_arr)));
    let tag = ctx.create_string(handler_tag);
    ctx.set_field(out, RE5_RESP_HANDLER_TAG, Value::Object(Some(tag)));
    out
}

/// Build the synthetic `java.net.http.HttpHeaders` view over a `String[]` of
/// `"key: value"` lines (the shape both the synthetic `HttpResponse` and the
/// synthetic `ResponseInfo` carry). The array is pinned across the
/// allocation so a moving collector can't leave the stored reference stale.
fn re5_make_http_headers(ctx: &mut dyn NativeContext, hdr_arr: Value) -> ObjectRef {
    let pinned = match hdr_arr {
        Value::Object(Some(a)) => Some((ctx.pin_native_root(a), a)),
        _ => None,
    };
    let headers = alloc_concurrent_synthetic(ctx, "java/net/http/HttpHeaders", 1);
    let arr_now = match pinned {
        Some((pin, a)) => Value::Object(Some(ctx.read_native_pin(pin, a))),
        None => Value::Object(None),
    };
    ctx.set_field(headers, 0, arr_now);
    if let Some((pin, _)) = pinned {
        ctx.unpin_native_roots(pin);
    }
    headers
}

/// `Flow.Subscription.request(long)` for the one-shot replay subscription
/// handed to real BodySubscribers by [`re5_drive_body_handler`]: on the
/// first positive demand, deliver the parked wire body as a single
/// `List.of(ByteBuffer)` `onNext` followed by `onComplete`, then drop the
/// parked references. Zero/negative or repeat demand is a no-op (the
/// reactive-streams spec says negative demand should `onError`; every JDK
/// `BodySubscriber` requests positive demand, so stay lenient).
///
/// Delivery is demand-driven rather than pushed eagerly from the driver
/// because not every `BodySubscriber` buffers: `BodySubscribers
/// .ofPublisher()`'s pass-through forwards items straight to a downstream
/// subscriber that only attaches (and signals demand) later, on a different
/// thread (Reactor, for Spring's reactive `JdkClientHttpConnector`) —
/// pushing before that demand would drop the body on the floor. All state
/// lives in the subscription's own Java fields (GC-traced), never in
/// cross-call native `ObjectRef`s.
fn re5_replay_subscription_request(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let n = match args.get(1).copied() {
        Some(Value::Long(v)) => v,
        Some(Value::Int(v)) => v as i64,
        _ => 0,
    };
    if n <= 0 || !matches!(ctx.get_field(this, RE5_SUB_STATE), Value::Int(0)) {
        return Ok(None);
    }
    // Claim delivery BEFORE invoking the subscriber: `onNext` commonly
    // re-enters `request` (e.g. `HttpResponseInputStream` requests the next
    // list while consuming the current one).
    ctx.set_field(this, RE5_SUB_STATE, Value::Int(1));
    let subscriber = match ctx.get_field(this, RE5_SUB_SUBSCRIBER) {
        Value::Object(Some(s)) => s,
        _ => return Ok(None),
    };
    let body = ctx.get_field(this, RE5_SUB_BODY);
    let this_pin = ctx.pin_native_root(this);
    let subscriber_pin = ctx.pin_native_root(subscriber);
    let deliver = |ctx: &mut dyn NativeContext| -> MethodCallResult {
        if let Value::Object(Some(arr)) = body {
            if ctx.array_length(arr) > 0 {
                let arr_pin = ctx.pin_native_root(arr);
                let arr_now = ctx.read_native_pin(arr_pin, arr);
                let bb = match ctx.invoke(
                    "java/nio/ByteBuffer",
                    "wrap",
                    "([B)Ljava/nio/ByteBuffer;",
                    &[Value::Object(Some(arr_now))],
                )? {
                    Some(Value::Object(Some(b))) => b,
                    _ => return Err(ioex("ByteBuffer.wrap returned null")),
                };
                let bb_pin = ctx.pin_native_root(bb);
                let bb_now = ctx.read_native_pin(bb_pin, bb);
                let list = match ctx.invoke(
                    "java/util/Collections",
                    "singletonList",
                    "(Ljava/lang/Object;)Ljava/util/List;",
                    &[Value::Object(Some(bb_now))],
                )? {
                    Some(v @ Value::Object(Some(_))) => v,
                    _ => return Err(ioex("Collections.singletonList returned null")),
                };
                let subscriber_now = ctx.read_native_pin(subscriber_pin, subscriber);
                // BodySubscriber<T> extends Flow.Subscriber<List<ByteBuffer>>;
                // the erased/bridge signature takes Object.
                ctx.invoke_virtual(subscriber_now, "onNext", "(Ljava/lang/Object;)V", &[list])?;
            }
        }
        let subscriber_now = ctx.read_native_pin(subscriber_pin, subscriber);
        ctx.invoke_virtual(subscriber_now, "onComplete", "()V", &[])?;
        Ok(None)
    };
    let result = deliver(ctx);
    // Drop the parked references so the delivered body can be collected.
    let this_now = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this_now, RE5_SUB_SUBSCRIBER, Value::Object(None));
    ctx.set_field(this_now, RE5_SUB_BODY, Value::Object(None));
    ctx.unpin_native_roots(this_pin);
    result
}

/// `Flow.Subscription.cancel()` for the one-shot replay subscription: mark
/// cancelled and drop the parked references. Idempotent; a cancel after
/// delivery is a no-op (the fields are already cleared).
fn re5_replay_subscription_cancel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if matches!(ctx.get_field(this, RE5_SUB_STATE), Value::Int(0)) {
        ctx.set_field(this, RE5_SUB_STATE, Value::Int(2));
        ctx.set_field(this, RE5_SUB_SUBSCRIBER, Value::Object(None));
        ctx.set_field(this, RE5_SUB_BODY, Value::Object(None));
    }
    Ok(None)
}

/// Drive the real `java.net.http` BodyHandler protocol against a
/// user-supplied handler, exactly as the real client would:
///
/// ```text
/// subscriber = handler.apply(responseInfo)
/// subscriber.onSubscribe(replaySubscription)   // request(n) pulls the body
/// body = subscriber.getBody().toCompletableFuture().join()
/// ```
///
/// Returns the handler-produced body value `T` (e.g. a `GZIPInputStream`
/// for Spring's `DecompressingBodyHandler`, a `Flow.Publisher` for
/// `BodyHandlers.ofPublisher()`), which `HttpResponse.body()` then hands
/// back verbatim (tag [`RE5_TAG_HANDLED`]).
///
/// `join()` cannot deadlock for the JDK's own subscriber shapes:
/// `ofInputStream`/`ofPublisher` complete their `getBody()` stage
/// immediately/at `onSubscribe`, and eagerly-buffering shapes (`ofString`,
/// `ofByteArray`) signal demand during `onSubscribe`, which makes the
/// replay subscription deliver the whole body + `onComplete` before
/// `getBody()` is even called.
fn re5_drive_body_handler(
    ctx: &mut dyn NativeContext,
    handler: ObjectRef,
    status: i32,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<Value, MethodCallFailed> {
    let handler_pin = ctx.pin_native_root(handler);

    // ResponseInfo synthetic: statusCode + the same "key: value" String[]
    // shape the synthetic HttpResponse carries.
    let ri = alloc_concurrent_synthetic(ctx, RE5_RESPONSE_INFO, RE5_RI_NUM_FIELDS);
    ctx.set_field(ri, RE5_RI_STATUS, Value::Int(status));
    let ri_pin = ctx.pin_native_root(ri);
    let hdr_arr = ctx.new_ref_array(ClassId::new(0), headers.len());
    let hdr_pin = ctx.pin_native_root(hdr_arr);
    for (i, (k, v)) in headers.iter().enumerate() {
        let s = ctx.create_string(&format!("{k}: {v}"));
        let hdr_now = ctx.read_native_pin(hdr_pin, hdr_arr);
        ctx.set_array_element(hdr_now, i, Value::Object(Some(s)));
    }
    {
        let ri_now = ctx.read_native_pin(ri_pin, ri);
        let hdr_now = ctx.read_native_pin(hdr_pin, hdr_arr);
        ctx.set_field(ri_now, RE5_RI_HEADERS, Value::Object(Some(hdr_now)));
    }

    // subscriber = handler.apply(responseInfo)
    let subscriber = {
        let handler_now = ctx.read_native_pin(handler_pin, handler);
        let ri_now = ctx.read_native_pin(ri_pin, ri);
        match ctx.invoke_virtual(
            handler_now,
            "apply",
            "(Ljava/net/http/HttpResponse$ResponseInfo;)Ljava/net/http/HttpResponse$BodySubscriber;",
            &[Value::Object(Some(ri_now))],
        )? {
            Some(Value::Object(Some(s))) => s,
            _ => return Err(ioex("HttpResponse.BodyHandler.apply returned null")),
        }
    };
    let subscriber_pin = ctx.pin_native_root(subscriber);

    // Park subscriber + body bytes in the replay subscription's own Java
    // fields (GC-traced; delivery may happen later, on another thread).
    let body_arr = if body.is_empty() {
        None
    } else {
        let arr = ctx.new_array(ArrayElementType::Byte, body.len());
        for (i, b) in body.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i32));
        }
        Some((ctx.pin_native_root(arr), arr))
    };
    let subscription = alloc_concurrent_synthetic(ctx, RE5_REPLAY_SUBSCRIPTION, RE5_SUB_NUM_FIELDS);
    {
        let subscriber_now = ctx.read_native_pin(subscriber_pin, subscriber);
        ctx.set_field(
            subscription,
            RE5_SUB_SUBSCRIBER,
            Value::Object(Some(subscriber_now)),
        );
        let body_val = match body_arr {
            Some((pin, a)) => Value::Object(Some(ctx.read_native_pin(pin, a))),
            None => Value::Object(None),
        };
        ctx.set_field(subscription, RE5_SUB_BODY, body_val);
        ctx.set_field(subscription, RE5_SUB_STATE, Value::Int(0));
    }
    let subscription_pin = ctx.pin_native_root(subscription);
    {
        let subscriber_now = ctx.read_native_pin(subscriber_pin, subscriber);
        let subscription_now = ctx.read_native_pin(subscription_pin, subscription);
        ctx.invoke_virtual(
            subscriber_now,
            "onSubscribe",
            "(Ljava/util/concurrent/Flow$Subscription;)V",
            &[Value::Object(Some(subscription_now))],
        )?;
    }

    // body = subscriber.getBody().toCompletableFuture().join()
    let stage = {
        let subscriber_now = ctx.read_native_pin(subscriber_pin, subscriber);
        match ctx.invoke_virtual(
            subscriber_now,
            "getBody",
            "()Ljava/util/concurrent/CompletionStage;",
            &[],
        )? {
            Some(Value::Object(Some(s))) => s,
            _ => return Err(ioex("BodySubscriber.getBody returned null")),
        }
    };
    // `getBody()` may return a CompletableFuture.MinimalStage, whose
    // `join()` throws UnsupportedOperationException -- always convert via
    // `toCompletableFuture()` first.
    let stage_pin = ctx.pin_native_root(stage);
    let cf = {
        let stage_now = ctx.read_native_pin(stage_pin, stage);
        match ctx.invoke_virtual(
            stage_now,
            "toCompletableFuture",
            "()Ljava/util/concurrent/CompletableFuture;",
            &[],
        )? {
            Some(Value::Object(Some(c))) => c,
            _ => return Err(ioex("CompletionStage.toCompletableFuture returned null")),
        }
    };
    let result = ctx.invoke_virtual(cf, "join", "()Ljava/lang/Object;", &[])?;
    ctx.unpin_native_roots(handler_pin);
    Ok(result.unwrap_or(Value::Object(None)))
}

const RE5_BODY_COLLECTOR_SUBSCRIBER: &str = "java/util/concurrent/Flow$Subscriber";
const RE5_PUBLISHER_WAIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Re5PublisherBodyState {
    bytes: Vec<u8>,
    completed: bool,
    error: Option<String>,
    // Identity-preserving companion to `error`: a global GC root for the
    // ORIGINAL Throwable the Flow.Subscriber's onError delivered (set
    // alongside `error` by `re5_body_collector_on_error`, which runs on a
    // different Java thread than the one waiting in
    // `re5_collect_publisher_body`, hence a global root rather than a pin).
    // Real HotSpot propagates a request-body-publisher failure through
    // `HttpClient.sendAsync()`'s CompletableFuture as the SAME exception
    // object the publisher threw (confirmed empirically: `ClientHttpConnectorTests
    // .errorInRequestBody`'s `assertThat(throwable).isSameAs(error)` passes
    // 3/3 clean under real JDK 25). Without this, `re5_collect_publisher_body`
    // could only reconstruct a brand-new synthetic `IOException` from a text
    // message, which can never satisfy an identity (`isSameAs`) assertion --
    // not a timing flake, a structural identity loss for every request that
    // fails this way on the `Jdk` connector.
    error_obj_root: Option<usize>,
}

#[derive(Default)]
struct Re5PublisherBodyCollector {
    state: StdMutex<Re5PublisherBodyState>,
    done: Condvar,
}

fn re5_body_collectors() -> &'static Mutex<HashMap<u64, Arc<Re5PublisherBodyCollector>>> {
    static COLLECTORS: OnceLock<Mutex<HashMap<u64, Arc<Re5PublisherBodyCollector>>>> =
        OnceLock::new();
    COLLECTORS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn re5_next_body_collector_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed).max(1)
}

fn re5_body_collector_id(ctx: &dyn NativeContext, subscriber: ObjectRef) -> Option<u64> {
    match ctx.get_field(subscriber, 0) {
        Value::Long(v) if v > 0 => Some(v as u64),
        Value::Int(v) if v > 0 => Some(v as u64),
        _ => None,
    }
}

fn re5_lookup_body_collector(
    ctx: &dyn NativeContext,
    subscriber: ObjectRef,
) -> Option<Arc<Re5PublisherBodyCollector>> {
    let id = re5_body_collector_id(ctx, subscriber)?;
    re5_body_collectors().lock().get(&id).cloned()
}

fn re5_is_byte_array(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    ctx.heap_kind_of(obj) == ObjectKind::Array
        && ctx.heap_element_type_of(obj) == ArrayElementType::Byte
}

fn re5_set_bytebuffer_position(ctx: &dyn NativeContext, bb: ObjectRef, pos: i32) {
    ctx.set_field_by_name(bb, "position", Value::Int(pos));
    ctx.set_field(bb, 1, Value::Int(pos));
}

fn re5_read_bytebuffer_fields(ctx: &dyn NativeContext, bb: ObjectRef) -> Option<Vec<u8>> {
    if let Value::Object(Some(arr)) = ctx.get_field_by_name(bb, "hb") {
        let pos = ctx.get_field_by_name(bb, "position").as_int().unwrap_or(0);
        let lim = ctx.get_field_by_name(bb, "limit").as_int().unwrap_or(pos);
        let off = ctx.get_field_by_name(bb, "offset").as_int().unwrap_or(0);
        let start = off.saturating_add(pos).max(0) as usize;
        let len = lim.saturating_sub(pos).max(0) as usize;
        let out = re5_read_byte_array_range(ctx, arr, start, len);
        re5_set_bytebuffer_position(ctx, bb, lim);
        return Some(out);
    }

    let arr = match ctx.get_field(bb, 0) {
        Value::Object(Some(a)) if re5_is_byte_array(ctx, a) => a,
        _ => return None,
    };
    let pos = ctx.get_field(bb, 1).as_int().unwrap_or(0);
    let lim = ctx.get_field(bb, 2).as_int().unwrap_or(pos);
    let len = lim.saturating_sub(pos).max(0) as usize;
    let out = re5_read_byte_array_range(ctx, arr, pos.max(0) as usize, len);
    re5_set_bytebuffer_position(ctx, bb, lim);
    Some(out)
}

fn re5_read_and_consume_bytebuffer(
    ctx: &mut dyn NativeContext,
    bb: ObjectRef,
) -> Result<Vec<u8>, MethodCallFailed> {
    if let Some(bytes) = re5_read_bytebuffer_fields(ctx, bb) {
        return Ok(bytes);
    }

    let rem = match ctx.invoke_virtual(bb, "remaining", "()I", &[])? {
        Some(Value::Int(n)) if n > 0 => n as usize,
        _ => return Ok(Vec::new()),
    };
    let tmp = ctx.new_array(ArrayElementType::Byte, rem);
    let tmp_pin = ctx.pin_native_root(tmp);
    let get_result = ctx.invoke_virtual(
        bb,
        "get",
        "([B)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(tmp))],
    );
    let tmp = ctx.read_native_pin(tmp_pin, tmp);
    ctx.unpin_native_roots(tmp_pin);
    get_result?;
    Ok(re5_read_byte_array(ctx, tmp))
}

fn re5_body_item_bytes(
    ctx: &mut dyn NativeContext,
    item: Value,
) -> Result<Vec<u8>, MethodCallFailed> {
    let obj = match item {
        Value::Object(Some(o)) => o,
        _ => return Ok(Vec::new()),
    };
    if re5_is_byte_array(ctx, obj) {
        return Ok(re5_read_byte_array(ctx, obj));
    }

    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(obj))
        .unwrap_or_default();
    if class_name == "java/lang/String" {
        return Ok(ctx.read_string(obj).unwrap_or_default().into_bytes());
    }
    if class_name.contains("ByteBuffer") || class_name == "java/nio/ByteBuffer" {
        return re5_read_and_consume_bytebuffer(ctx, obj);
    }
    Ok(Vec::new())
}

fn re5_throwable_text(ctx: &mut dyn NativeContext, value: Value) -> String {
    let obj = match value {
        Value::Object(Some(o)) => o,
        _ => return "publisher signalled an error".to_string(),
    };
    if let Ok(Some(Value::Object(Some(s)))) =
        ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[])
    {
        if let Some(text) = ctx.read_string(s) {
            return text;
        }
    }
    ctx.class_name_of_id(ctx.class_id_of_object(obj))
        .unwrap_or_else(|| "publisher signalled an error".to_string())
}

fn re5_body_collector_on_subscribe(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(subscription))) = args.get(1).copied() {
        ctx.invoke_virtual(subscription, "request", "(J)V", &[Value::Long(i64::MAX)])?;
    }
    Ok(None)
}

fn re5_body_collector_on_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let bytes = re5_body_item_bytes(ctx, args.get(1).copied().unwrap_or(Value::Object(None)))?;
    if let Some(collector) = re5_lookup_body_collector(ctx, this) {
        let mut state = collector.state.lock().unwrap();
        state.bytes.extend_from_slice(&bytes);
        collector.done.notify_all();
    }
    Ok(None)
}

fn re5_body_collector_on_error(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let throwable_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let msg = re5_throwable_text(ctx, throwable_val);
    // Root the actual Throwable object (if any) so `re5_collect_publisher_body`
    // -- woken on a different thread, possibly after a GC moves it -- can
    // rethrow the SAME object instead of only a text description. A global
    // root (not a pin) is required: `on_error` and the collect/wait side run
    // on different Java threads, so there is no shared native-call frame to
    // pin against.
    let error_obj_root = match throwable_val {
        Value::Object(Some(obj)) => Some(ctx.add_global_root(obj)),
        _ => None,
    };
    if let Some(collector) = re5_lookup_body_collector(ctx, this) {
        let mut state = collector.state.lock().unwrap();
        state.error = Some(msg);
        state.error_obj_root = error_obj_root;
        state.completed = true;
        collector.done.notify_all();
    } else if let Some(handle) = error_obj_root {
        // No collector (already timed out / removed) to hand the root to --
        // avoid leaking it.
        ctx.remove_global_root(handle);
    }
    Ok(None)
}

fn re5_body_collector_on_complete(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(collector) = re5_lookup_body_collector(ctx, this) {
        let mut state = collector.state.lock().unwrap();
        state.completed = true;
        collector.done.notify_all();
    }
    Ok(None)
}

fn re5_collect_publisher_body(
    ctx: &mut dyn NativeContext,
    publisher: ObjectRef,
) -> Result<Vec<u8>, MethodCallFailed> {
    let id = re5_next_body_collector_id();
    let collector = Arc::new(Re5PublisherBodyCollector::default());
    re5_body_collectors().lock().insert(id, collector.clone());

    let subscriber = alloc_concurrent_synthetic(ctx, RE5_BODY_COLLECTOR_SUBSCRIBER, 1);
    ctx.set_field(subscriber, 0, Value::Long(id as i64));
    let sub_global = ctx.add_global_root(subscriber);
    let pin = ctx.pin_native_root(publisher);
    let sub_pin = ctx.pin_native_root(subscriber);
    let subscribe_result = ctx.invoke_virtual(
        ctx.read_native_pin(pin, publisher),
        "subscribe",
        "(Ljava/util/concurrent/Flow$Subscriber;)V",
        &[Value::Object(Some(
            ctx.read_native_pin(sub_pin, subscriber),
        ))],
    );
    ctx.unpin_native_roots(pin);

    if let Err(e) = subscribe_result {
        re5_body_collectors().lock().remove(&id);
        if sub_global != 0 {
            ctx.remove_global_root(sub_global);
        }
        return Err(e);
    }

    // STW cross-thread JIT-takeover deadlock fix, same family as the
    // http_perform_request fix in re5_do_request (see that comment for the
    // full mechanism): re5_collect_publisher_body's condvar wait blocks this
    // thread for up to RE5_PUBLISHER_WAIT waiting for a notify delivered by
    // a DIFFERENT Java thread (the Reactor scheduler thread driving the
    // Publisher, calling back into re5_body_collector_on_next/on_complete).
    // That signalling thread cooperates normally with an STW pause (it's
    // ordinary bytecode, hits interpreter safepoints); this thread, stuck in
    // a raw Rust condvar wait, does not -- so a concurrent STW request
    // starves waiting on THIS thread while the notify THIS thread needs
    // waits on the OTHER thread's own cooperation with that same pause.
    let deadline = Instant::now() + RE5_PUBLISHER_WAIT;
    ctx.begin_blocking_region();
    let mut state = collector.state.lock().unwrap_or_else(|e| e.into_inner());
    while !state.completed && state.error.is_none() {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait_for = deadline.saturating_duration_since(now);
        let (next_state, wait) = collector
            .done
            .wait_timeout(state, wait_for)
            .unwrap_or_else(|e| e.into_inner());
        state = next_state;
        if wait.timed_out() {
            break;
        }
    }
    ctx.end_blocking_region();

    let timed_out = !state.completed && state.error.is_none();
    let error = state.error.clone();
    let error_obj_root = state.error_obj_root.take();
    let out = state.bytes.clone();
    drop(state);
    re5_body_collectors().lock().remove(&id);
    if sub_global != 0 {
        ctx.remove_global_root(sub_global);
    }

    if re5_dbg() {
        eprintln!(
            "[RE5-DBG] re5_collect_publisher_body id={id} timed_out={timed_out} bytes={} error={:?}",
            out.len(), error
        );
    }
    if timed_out {
        if let Some(handle) = error_obj_root {
            ctx.remove_global_root(handle);
        }
        return Err(ioex("HttpRequest body publisher did not complete"));
    }
    if error.is_some() {
        // Prefer rethrowing the ORIGINAL Throwable (identity-preserving,
        // matching real JDK's observed behaviour) over synthesizing a new
        // IOException from just its text. Resolution can fail if the root
        // somehow never got set; fall back to the old text-only wrapping
        // rather than silently swallowing the failure.
        if let Some(handle) = error_obj_root {
            let resolved = ctx.resolve_global_root(handle);
            ctx.remove_global_root(handle);
            if let Some(orig) = resolved {
                return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                    orig,
                ));
            }
        }
        let msg = error.unwrap();
        return Err(ioex(format!("HttpRequest body publisher failed: {msg}")));
    }
    Ok(out)
}

fn re5_request_body_bytes(
    ctx: &mut dyn NativeContext,
    value: Value,
) -> Result<Vec<u8>, MethodCallFailed> {
    let obj = match value {
        Value::Object(Some(o)) => o,
        _ => return Ok(Vec::new()),
    };
    if re5_is_byte_array(ctx, obj) {
        return Ok(re5_read_byte_array(ctx, obj));
    }

    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(obj))
        .unwrap_or_default();
    if class_name == "java/lang/String" {
        return Ok(ctx.read_string(obj).unwrap_or_default().into_bytes());
    }
    if class_name.contains("ByteBuffer") || class_name == "java/nio/ByteBuffer" {
        return re5_read_and_consume_bytebuffer(ctx, obj);
    }
    re5_collect_publisher_body(ctx, obj)
}

const RE5_REQUEST_TIMEOUT_FIELD: usize = 4;

fn re5_request_timeout(
    ctx: &mut dyn NativeContext,
    request: ObjectRef,
) -> Result<Duration, MethodCallFailed> {
    let duration = match ctx.get_field(request, RE5_REQUEST_TIMEOUT_FIELD) {
        Value::Object(Some(duration)) => duration,
        _ => return Ok(Duration::from_secs(30)),
    };
    // Read the Duration through its public method instead of assuming the
    // real-JDK object's field layout.
    let millis = match ctx.invoke_virtual(duration, "toMillis", "()J", &[])? {
        Some(Value::Long(millis)) => millis,
        Some(Value::Int(millis)) => millis as i64,
        _ => 0,
    };
    // A positive sub-millisecond Duration has a toMillis() value of zero, so
    // use the smallest timeout the operating-system socket API can represent.
    Ok(Duration::from_millis(millis.max(1) as u64))
}

/// Shared request driver for `HttpClient.send` / `sendAsync`. `args[0]` is the
/// `HttpClient`, `args[1]` the `HttpRequest`, `args[2]` the `BodyHandler`.
fn re5_do_request(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let req = obj_arg(args, 1)?;
    let handler_val = args.get(2).copied();
    let handler_tag = re5_handler_tag(ctx, handler_val);
    let method = read_field_string_or(ctx, req, 0, "GET");
    let uri = read_field_string_or(ctx, req, 1, "");
    let request_timeout = re5_request_timeout(ctx, req)?;
    if re5_dbg() {
        eprintln!("[RE5-DBG] re5_do_request ENTER method={method} uri={uri}");
    }
    let body_val = ctx.get_field(req, 2);
    let body = re5_request_body_bytes(ctx, body_val)?;
    let hdrs_val = ctx.get_field(req, 3);
    let headers = huc_extract_req_headers(ctx, hdrs_val);
    if uri.is_empty() {
        return Err(ioex("HttpRequest.uri is empty"));
    }
    // A real (non-synthetic) BodyHandler must survive the blocking exchange:
    // the moving collector can run from other threads while this thread is
    // off in socket I/O, so pin it for the duration.
    let real_handler = match (handler_tag.as_deref(), handler_val) {
        (None, Some(Value::Object(Some(h)))) => Some((ctx.pin_native_root(h), h)),
        _ => None,
    };
    if re5_dbg() {
        eprintln!(
            "[RE5-DBG] re5_do_request method={method} uri={uri} body_len={} calling http_perform_request...",
            body.len()
        );
    }
    // STW cross-thread JIT-takeover deadlock fix (found investigating
    // reactive ClientHttpConnectorTests intermittent hangs, 2026-07-15): the
    // raw TcpStream connect/write/read cycle inside `http_perform_request`
    // blocks this thread in a genuine OS syscall for up to 30s (its own
    // socket-level read timeout) without cooperating with a concurrent
    // Stop-The-World pause -- unlike every OTHER blocking native I/O call in
    // this file (see `re1_socket_read_stream`/`re1_socket_write_stream`
    // above), which correctly brackets the syscall with
    // `begin_blocking_region`/`end_blocking_region` so the GC barrier
    // excludes this thread from `expected` while it cannot reach a
    // safepoint. Without that, a concurrent STW request (e.g. a JIT
    // recompile or GC pause triggered by unrelated activity in the SAME
    // process) waits up to its full round budget for this thread to
    // cooperate -- while the STW pause is simultaneously what freezes the
    // real Java thread on the OTHER end of the socket (MockWebServer's own
    // response-writing dispatcher, ordinary bytecode running in this same
    // JVM process) that this thread is blocked waiting to hear from. Live
    // capture: `CRATONVM_DBG_RE5=1` showed a `DELETE` request's `write_all`
    // succeed in under 200us, then `read_response` block for the full 30s
    // socket timeout and fail with `WouldBlock`, with a
    // "STW cross-thread JIT takeover is still waiting for cooperative
    // mutators rounds=64 pending=1 taken=0" warning firing mid-block --
    // confirmed HotSpot-only-divergent (8/8 clean runs of the identical
    // 32-request sequential-MockWebServer-cycle probe on HotSpot; CratonVM
    // hit it on ~2/13 attempts, always on this JDK-connector code path,
    // never on the Reactor-Netty/Jetty/HttpComponents connectors that don't
    // route through this raw-socket implementation).
    ctx.begin_blocking_region();
    let perform_result =
        http_perform_request_with_timeout(&method, &uri, &headers, &body, request_timeout, 10);
    ctx.end_blocking_region();
    let resp = perform_result.map_err(|e| {
        if re5_dbg() {
            eprintln!("[RE5-DBG] re5_do_request method={method} uri={uri} http_perform_request FAILED: {e}");
        }
        if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
            ioex("HttpClient request timed out")
        } else {
            ioex(format!("HttpClient request failed: {e}"))
        }
    })?;
    if re5_dbg() {
        eprintln!(
            "[RE5-DBG] re5_do_request method={method} uri={uri} http_perform_request OK status={}",
            resp.status
        );
    }
    let out = match real_handler {
        None => {
            let tag = handler_tag.unwrap_or_else(|| "inputstream".to_string());
            re5_build_response(ctx, resp.status, &resp.headers, &resp.body, &tag)
        }
        Some((handler_pin, handler)) => {
            // Drive the user's BodyHandler protocol against the wire bytes;
            // body() then returns whatever value the handler produced.
            let handler_now = ctx.read_native_pin(handler_pin, handler);
            let body_obj =
                re5_drive_body_handler(ctx, handler_now, resp.status, &resp.headers, &resp.body)?;
            let body_obj_pin = match body_obj {
                Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
                _ => None,
            };
            let out =
                re5_build_response(ctx, resp.status, &resp.headers, &resp.body, RE5_TAG_HANDLED);
            let body_obj_now = match body_obj_pin {
                Some((pin, o)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
                None => Value::Object(None),
            };
            ctx.set_field(out, RE5_RESP_BODY_OBJ, body_obj_now);
            ctx.unpin_native_roots(handler_pin);
            out
        }
    };
    Ok(Some(Value::Object(Some(out))))
}

/// Extract the full external-form string from a `java.net.URI` and return it as
/// a heap String. `java.net.URI`'s slot 0 is the `scheme` ("http"), not the
/// whole URL, so reading the field directly is wrong; `uri_raw_string` is the
/// module's slot-order-safe reconstruction (reads the `string` cache field by
/// name, the same path every URI getter uses) and works for both a real JDK
/// `URI` and a `make_uri`-built one. Falls back to `toString()` only if that
/// yields nothing.
fn re5_uri_string(ctx: &mut dyn NativeContext, uri: ObjectRef) -> ObjectRef {
    // 1. Cached external form (the `string` field, slot-order-safe).
    let raw = uri_raw_string(ctx, uri);
    if !raw.is_empty() {
        return ctx.create_string(&raw);
    }
    // 2. Reconstruct from the URI's own getters (proven natives that read named
    //    components), so we don't depend on the `string` cache being populated.
    let getter = |ctx: &mut dyn NativeContext, m: &str, d: &str| -> Option<String> {
        match ctx.invoke_virtual(uri, m, d, &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).filter(|v| !v.is_empty()),
            _ => None,
        }
    };
    if let Some(scheme) = getter(ctx, "getScheme", "()Ljava/lang/String;") {
        let mut url = format!("{scheme}://");
        if let Some(host) = getter(ctx, "getHost", "()Ljava/lang/String;") {
            url.push_str(&host);
            if let Ok(Some(Value::Int(port))) = ctx.invoke_virtual(uri, "getPort", "()I", &[]) {
                if port > 0 {
                    url.push_str(&format!(":{port}"));
                }
            }
        }
        if let Some(path) = getter(ctx, "getRawPath", "()Ljava/lang/String;") {
            url.push_str(&path);
        }
        if let Some(query) = getter(ctx, "getRawQuery", "()Ljava/lang/String;") {
            url.push('?');
            url.push_str(&query);
        }
        if url.contains("://") && url.len() > scheme.len() + 3 {
            return ctx.create_string(&url);
        }
    }
    // 3. Last resort: toString().
    if let Ok(Some(Value::Object(Some(s)))) =
        ctx.invoke_virtual(uri, "toString", "()Ljava/lang/String;", &[])
    {
        if let Some(text) = ctx.read_string(s) {
            if text.contains("://") || text.starts_with('/') {
                return s;
            }
        }
    }
    ctx.create_string("")
}

fn register_re5_http_client(r: &mut NativeMethodRegistry) {
    r.register(
        RE5_BODY_COLLECTOR_SUBSCRIBER,
        "onSubscribe",
        "(Ljava/util/concurrent/Flow$Subscription;)V",
        re5_body_collector_on_subscribe,
    );
    r.register(
        RE5_BODY_COLLECTOR_SUBSCRIBER,
        "onNext",
        "(Ljava/lang/Object;)V",
        re5_body_collector_on_next,
    );
    r.register(
        RE5_BODY_COLLECTOR_SUBSCRIBER,
        "onError",
        "(Ljava/lang/Throwable;)V",
        re5_body_collector_on_error,
    );
    r.register(
        RE5_BODY_COLLECTOR_SUBSCRIBER,
        "onComplete",
        "()V",
        re5_body_collector_on_complete,
    );

    let hc = "java/net/http/HttpClient";
    r.register(
        hc,
        "newHttpClient",
        "()Ljava/net/http/HttpClient;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient", 1);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
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
    r.register(
        bld,
        "build",
        "()Ljava/net/http/HttpClient;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient", 1);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
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

    // These are abstract interface methods. In the real-JDK build `newBuilder`
    // creates this one-field synthetic directly, so every fluent method must be
    // registered here rather than in the synthetic-only HTTP/2 registrar.
    for (method, descriptor) in [
        (
            "version",
            "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpClient$Builder;",
        ),
        ("priority", "(I)Ljava/net/http/HttpClient$Builder;"),
        (
            "executor",
            "(Ljava/util/concurrent/Executor;)Ljava/net/http/HttpClient$Builder;",
        ),
        (
            "cookieHandler",
            "(Ljava/net/CookieHandler;)Ljava/net/http/HttpClient$Builder;",
        ),
        (
            "proxy",
            "(Ljava/net/ProxySelector;)Ljava/net/http/HttpClient$Builder;",
        ),
        (
            "authenticator",
            "(Ljava/net/Authenticator;)Ljava/net/http/HttpClient$Builder;",
        ),
        (
            "sslContext",
            "(Ljavax/net/ssl/SSLContext;)Ljava/net/http/HttpClient$Builder;",
        ),
        (
            "sslParameters",
            "(Ljavax/net/ssl/SSLParameters;)Ljava/net/http/HttpClient$Builder;",
        ),
    ] {
        r.register(bld, method, descriptor, |_ctx, args| Ok(Some(args[0])));
    }

    r.register(
        hc,
        "send",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;",
        |ctx, args| re5_do_request(ctx, args),
    );

    // sendAsync(HttpRequest, BodyHandler) -> CompletableFuture<HttpResponse>.
    // Spring's `JdkClientHttpRequest` (RestClient / JdkClientHttpRequestFactory)
    // drives requests exclusively through sendAsync(...).get(). We perform the
    // request synchronously and hand back an already-completed real
    // CompletableFuture so the caller's `.get()` returns immediately.
    let send_async: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = |ctx, args| {
        let resp = re5_do_request(ctx, args)?.unwrap_or(Value::Object(None));
        ctx.invoke(
            "java/util/concurrent/CompletableFuture",
            "completedFuture",
            "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;",
            &[resp],
        )
    };
    r.register(
        hc,
        "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;",
        send_async,
    );
    r.register(
        hc,
        "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;Ljava/net/http/HttpResponse$PushPromiseHandler;)Ljava/util/concurrent/CompletableFuture;",
        send_async,
    );

    // --- HttpClient instance accessors (RE.5 audit) ---------------------------
    // `java.net.http.HttpClient` declares these as abstract; on the synthetic
    // bare-client object they have neither real bytecode nor a native, so any
    // call throws `AbstractMethodError: ... has no Code attribute`. Spring's
    // `JdkClientHttpRequestFactory` ctor calls `executor()`; the rest are filled
    // for parity so they degrade to JDK-default values instead of crashing.
    let opt_empty: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |ctx, _args| ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[]);
    r.register(hc, "executor", "()Ljava/util/Optional;", opt_empty);
    r.register(hc, "connectTimeout", "()Ljava/util/Optional;", opt_empty);
    r.register(hc, "proxy", "()Ljava/util/Optional;", opt_empty);
    r.register(hc, "authenticator", "()Ljava/util/Optional;", opt_empty);
    r.register(hc, "cookieHandler", "()Ljava/util/Optional;", opt_empty);
    // version() -> HttpClient.Version (default HTTP_2); fetch the real enum
    // constant so the returned object is a genuine Version, not an int proxy.
    r.register(
        hc,
        "version",
        "()Ljava/net/http/HttpClient$Version;",
        |ctx, _args| {
            let name = ctx.create_string("HTTP_2");
            ctx.invoke(
                "java/net/http/HttpClient$Version",
                "valueOf",
                "(Ljava/lang/String;)Ljava/net/http/HttpClient$Version;",
                &[Value::Object(Some(name))],
            )
        },
    );
    // followRedirects() -> HttpClient.Redirect (newHttpClient() default: NEVER).
    r.register(
        hc,
        "followRedirects",
        "()Ljava/net/http/HttpClient$Redirect;",
        |ctx, _args| {
            let name = ctx.create_string("NEVER");
            ctx.invoke(
                "java/net/http/HttpClient$Redirect",
                "valueOf",
                "(Ljava/lang/String;)Ljava/net/http/HttpClient$Redirect;",
                &[Value::Object(Some(name))],
            )
        },
    );
    // sslContext() -> the JVM default SSLContext.
    r.register(
        hc,
        "sslContext",
        "()Ljavax/net/ssl/SSLContext;",
        |ctx, _args| {
            ctx.invoke(
                "javax/net/ssl/SSLContext",
                "getDefault",
                "()Ljavax/net/ssl/SSLContext;",
                &[],
            )
        },
    );
    // close()/shutdown()/shutdownNow() (JDK 21+ AutoCloseable surface) — no-ops;
    // our synthetic client owns no background selector/threads to tear down.
    for (m, d) in [
        ("close", "()V"),
        ("shutdown", "()V"),
        ("shutdownNow", "()V"),
    ] {
        r.register(hc, m, d, |_ctx, _args| Ok(None));
    }
    r.register(hc, "isTerminated", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    r.register(
        hc,
        "awaitTermination",
        "(Ljava/time/Duration;)Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
    );

    let req = "java/net/http/HttpRequest";
    r.register(
        req,
        "newBuilder",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, _args| {
            let b = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$Builder", 5);
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
            let b = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$Builder", 5);
            let m = ctx.create_string("GET");
            ctx.set_field(b, 0, Value::Object(Some(m)));
            let uri = obj_arg(args, 0)?;
            let uri_s = re5_uri_string(ctx, uri);
            ctx.set_field(b, 1, Value::Object(Some(uri_s)));
            ctx.set_field(b, 2, Value::Object(None));
            ctx.set_field(b, 3, Value::Object(None));
            Ok(Some(Value::Object(Some(b))))
        },
    );
    r.register(req, "timeout", "()Ljava/util/Optional;", |ctx, args| {
        let request = obj_arg(args, 0)?;
        match ctx.get_field(request, RE5_REQUEST_TIMEOUT_FIELD) {
            Value::Object(Some(timeout)) => ctx.invoke(
                "java/util/Optional",
                "of",
                "(Ljava/lang/Object;)Ljava/util/Optional;",
                &[Value::Object(Some(timeout))],
            ),
            _ => ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[]),
        }
    });

    let bl = "java/net/http/HttpRequest$Builder";
    r.register(
        bl,
        "uri",
        "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let uri = obj_arg(args, 1)?;
            let uri_s = re5_uri_string(ctx, uri);
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
    // method(String, BodyPublisher) — the generic verb setter Spring uses for
    // POST/PUT/PATCH (and any custom verb). Slot 0 = method name, slot 2 = body
    // (carried only for literal publishers; see `re5_do_request`).
    r.register(
        bl,
        "method",
        "(Ljava/lang/String;Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(m @ Value::Object(Some(_))) = args.get(1).copied() {
                ctx.set_field(this, 0, m);
            }
            if let Some(Value::Object(Some(bp))) = args.get(2) {
                let body = ctx.get_field(*bp, 0);
                ctx.set_field(this, 2, body);
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );
    // timeout(Duration) / expectContinue(boolean) / version(Version) — accepted
    // and chained, but not separately modelled (request timeout is enforced by
    // the caller; the bare client speaks HTTP/1.1). Returning `this` keeps the
    // fluent builder chain intact instead of throwing AbstractMethodError.
    r.register(
        bl,
        "timeout",
        "(Ljava/time/Duration;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let timeout = obj_arg(args, 1)?;
            let is_zero = matches!(
                ctx.invoke_virtual(timeout, "isZero", "()Z", &[])?,
                Some(Value::Int(value)) if value != 0
            );
            let is_negative = matches!(
                ctx.invoke_virtual(timeout, "isNegative", "()Z", &[])?,
                Some(Value::Int(value)) if value != 0
            );
            if is_zero || is_negative {
                return Err(iae("HttpRequest timeout must be positive"));
            }
            ctx.set_field(
                this,
                RE5_REQUEST_TIMEOUT_FIELD,
                Value::Object(Some(timeout)),
            );
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        bl,
        "expectContinue",
        "(Z)Ljava/net/http/HttpRequest$Builder;",
        |_ctx, args| Ok(Some(args[0])),
    );
    r.register(
        bl,
        "version",
        "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpRequest$Builder;",
        |_ctx, args| Ok(Some(args[0])),
    );

    r.register(bl, "build", "()Ljava/net/http/HttpRequest;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let req = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest", 5);
        for i in 0..5 {
            let v = ctx.get_field(this, i);
            ctx.set_field(req, i, v);
        }
        Ok(Some(Value::Object(Some(req))))
    });

    let bps = "java/net/http/HttpRequest$BodyPublishers";
    r.register(
        bps,
        "ofString",
        "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, args| {
            let body =
                alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$BodyPublisher", 1);
            ctx.set_field(
                body,
                0,
                args.first().copied().unwrap_or(Value::Object(None)),
            );
            Ok(Some(Value::Object(Some(body))))
        },
    );
    r.register(
        bps,
        "noBody",
        "()Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, _args| {
            let body =
                alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$BodyPublisher", 1);
            let empty = ctx.create_string("");
            ctx.set_field(body, 0, Value::Object(Some(empty)));
            Ok(Some(Value::Object(Some(body))))
        },
    );
    r.register(
        bps,
        "ofByteArray",
        "([B)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, args| {
            let body =
                alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$BodyPublisher", 1);
            let bytes = match args.first().copied() {
                Some(Value::Object(Some(arr))) => {
                    String::from_utf8_lossy(&re5_read_byte_array(ctx, arr)).into_owned()
                }
                _ => String::new(),
            };
            let s = ctx.create_string(&bytes);
            ctx.set_field(body, 0, Value::Object(Some(s)));
            Ok(Some(Value::Object(Some(body))))
        },
    );
    // fromPublisher(Flow.Publisher[, contentLength]) — Spring's streaming
    // POST/PUT path. Keep the publisher object in slot 0; `re5_do_request`
    // subscribes a native collector, requests demand, and assembles the emitted
    // ByteBuffers into the wire body before opening the socket.
    for desc in [
        "(Ljava/util/concurrent/Flow$Publisher;)Ljava/net/http/HttpRequest$BodyPublisher;",
        "(Ljava/util/concurrent/Flow$Publisher;J)Ljava/net/http/HttpRequest$BodyPublisher;",
    ] {
        r.register(bps, "fromPublisher", desc, |ctx, args| {
            let body =
                alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$BodyPublisher", 1);
            ctx.set_field(
                body,
                0,
                args.first().copied().unwrap_or(Value::Object(None)),
            );
            Ok(Some(Value::Object(Some(body))))
        });
    }

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
    // ofInputStream() / ofByteArray() — the body shapes Spring's
    // `JdkClientHttpRequest` consumes (it always reads `response.body()` as an
    // InputStream). The tag drives `HttpResponse.body()` materialisation.
    r.register(
        bhs,
        "ofInputStream",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse$BodyHandler", 1);
            let t = ctx.create_string("inputstream");
            ctx.set_field(bh, 0, Value::Object(Some(t)));
            Ok(Some(Value::Object(Some(bh))))
        },
    );
    r.register(
        bhs,
        "ofByteArray",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse$BodyHandler", 1);
            let t = ctx.create_string("bytearray");
            ctx.set_field(bh, 0, Value::Object(Some(t)));
            Ok(Some(Value::Object(Some(bh))))
        },
    );

    let resp = "java/net/http/HttpResponse";
    r.register(resp, "statusCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, RE5_RESP_STATUS)))
    });
    // body() materialises the stored byte[] per the BodyHandler tag captured at
    // send time: an InputStream (default / ofInputStream), a String (ofString),
    // the raw byte[] (ofByteArray), null (discarding), or -- for a real
    // user-supplied BodyHandler -- the exact value the driven handler's
    // BodySubscriber produced (RE5_TAG_HANDLED; same instance every call,
    // matching the real client).
    r.register(resp, "body", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tag = read_field_string_or(ctx, this, RE5_RESP_HANDLER_TAG, "inputstream");
        let body_val = ctx.get_field(this, RE5_RESP_BODY_BYTES);
        match tag.as_str() {
            RE5_TAG_HANDLED => Ok(Some(ctx.get_field(this, RE5_RESP_BODY_OBJ))),
            "discarding" => Ok(Some(Value::Object(None))),
            "bytearray" => Ok(Some(body_val)),
            "string" => {
                let bytes = match body_val {
                    Value::Object(Some(a)) => re5_read_byte_array(ctx, a),
                    _ => Vec::new(),
                };
                let s = ctx.create_string(&String::from_utf8_lossy(&bytes));
                Ok(Some(Value::Object(Some(s))))
            }
            _ => {
                // InputStream: wrap the byte[] in a real ByteArrayInputStream.
                let arr = match body_val {
                    Value::Object(Some(_)) => body_val,
                    _ => Value::Object(Some(ctx.new_array(ArrayElementType::Byte, 0))),
                };
                ctx.new_object_initialized("java/io/ByteArrayInputStream", "([B)V", &[arr])
            }
        }
    });
    // headers() -> HttpHeaders backed by the stored "key: value" String[].
    r.register(
        resp,
        "headers",
        "()Ljava/net/http/HttpHeaders;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = ctx.get_field(this, RE5_RESP_HEADERS);
            let headers = re5_make_http_headers(ctx, arr);
            Ok(Some(Value::Object(Some(headers))))
        },
    );

    // HttpResponse.ResponseInfo -- the argument re5_drive_body_handler hands
    // to a real BodyHandler's apply(). Same synthetic-receiver pattern as the
    // HttpResponse natives above.
    r.register(RE5_RESPONSE_INFO, "statusCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, RE5_RI_STATUS)))
    });
    r.register(
        RE5_RESPONSE_INFO,
        "headers",
        "()Ljava/net/http/HttpHeaders;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = ctx.get_field(this, RE5_RI_HEADERS);
            let headers = re5_make_http_headers(ctx, arr);
            Ok(Some(Value::Object(Some(headers))))
        },
    );
    r.register(
        RE5_RESPONSE_INFO,
        "version",
        "()Ljava/net/http/HttpClient$Version;",
        |ctx, _args| {
            // The bare client speaks HTTP/1.1.
            let name = ctx.create_string("HTTP_1_1");
            ctx.invoke(
                "java/net/http/HttpClient$Version",
                "valueOf",
                "(Ljava/lang/String;)Ljava/net/http/HttpClient$Version;",
                &[Value::Object(Some(name))],
            )
        },
    );

    // One-shot replay Flow.Subscription (see re5_drive_body_handler /
    // re5_replay_subscription_request). Registered on its own wrapper class
    // name; abstract Flow$Subscription dispatch finds receiver-class natives.
    r.register(
        RE5_REPLAY_SUBSCRIPTION,
        "request",
        "(J)V",
        re5_replay_subscription_request,
    );
    r.register(
        RE5_REPLAY_SUBSCRIPTION,
        "cancel",
        "()V",
        re5_replay_subscription_cancel,
    );

    // HttpHeaders.map() -> Map<String, List<String>>. Spring's
    // `JdkClientHttpResponse.adaptHeaders` calls `response.headers().map()` and
    // iterates it, so this must be a real Map of real Lists. Header names are
    // grouped case-insensitively, preserving first-seen order.
    let hh = "java/net/http/HttpHeaders";
    r.register(hh, "map", "()Ljava/util/Map;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Parse the stored "key: value" lines into insertion-ordered groups.
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            let n = ctx.array_length(arr);
            for i in 0..n {
                if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
                    if let Some(line) = ctx.read_string(s) {
                        if let Some(c) = line.find(':') {
                            let k = line[..c].trim().to_string();
                            let v = line[c + 1..].trim().to_string();
                            if k.is_empty() {
                                continue;
                            }
                            if let Some(g) = groups
                                .iter_mut()
                                .find(|(gk, _)| gk.eq_ignore_ascii_case(&k))
                            {
                                g.1.push(v);
                            } else {
                                groups.push((k, vec![v]));
                            }
                        }
                    }
                }
            }
        }
        let map_val = ctx.new_object_initialized("java/util/LinkedHashMap", "()V", &[])?;
        let map = match map_val {
            Some(Value::Object(Some(m))) => m,
            _ => return Ok(map_val),
        };
        // Pin the map across the (allocating) per-entry construction so a moving
        // collector can't invalidate the reference we keep `put`-ing into.
        let map_pin = ctx.pin_native_root(map);
        for (k, vals) in groups {
            let list_val = ctx.new_object_initialized("java/util/ArrayList", "()V", &[])?;
            let list = match list_val {
                Some(Value::Object(Some(l))) => l,
                _ => continue,
            };
            let list_pin = ctx.pin_native_root(list);
            for v in vals {
                let vs = ctx.create_string(&v);
                let list_now = ctx.read_native_pin(list_pin, list);
                ctx.invoke_virtual(
                    list_now,
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(vs))],
                )?;
            }
            let ks = ctx.create_string(&k);
            let list_now = ctx.read_native_pin(list_pin, list);
            let map_now = ctx.read_native_pin(map_pin, map);
            ctx.invoke_virtual(
                map_now,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(ks)), Value::Object(Some(list_now))],
            )?;
        }
        let map_now = ctx.read_native_pin(map_pin, map);
        ctx.unpin_native_roots(map_pin);
        Ok(Some(Value::Object(Some(map_now))))
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
            // FIX (es-restclientbuilder-ssl-default-context-20260710): if
            // `SSLContext.setDefault(ctx)` installed a context (see the new
            // `setDefault` registration below), return that SAME object --
            // not a fresh, unconfigured one -- so callers that rely on the
            // JDK's documented getDefault()/setDefault() contract (e.g.
            // RestClientBuilder.build() -> SSLContext.getDefault() at
            // RestClientBuilder.java:330) see the trust/key managers that
            // were attached to it at `init()` time. This is the LIVE
            // getDefault()/init() registration in real-JDK mode: it is
            // registered here, in `register_re6_ssl_context` (called from
            // `register_essential_natives`, unconditionally), AFTER
            // `phases_late::register_p68_ssl`'s own getDefault/init (called a
            // few lines earlier in the same function) -- so this
            // implementation wins via last-registered-wins. The THIRD and
            // FOURTH registrations of the same (class, method, descriptor)
            // triple, in `tls.rs::register_ssl_context` and a second call to
            // `phases_late::register_p68_ssl`, both live inside
            // `register_synthetic_overrides`, which is
            // `#[cfg(feature = "synthetic-jdk")]`-gated to a no-op shim in
            // the default real-JDK build -- so they never run here and are
            // not live competitors for last-write-wins.
            if let Some(ctx_obj) = crate::t27_tls::get_runtime_default_ssl_context() {
                return Ok(Some(Value::Object(Some(ctx_obj))));
            }
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", 2);
            let name = ctx.create_string("TLS");
            ctx.set_field(obj, 0, Value::Object(Some(name)));
            ctx.set_field(obj, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    // `SSLContext.setDefault(SSLContext)` -- previously missing entirely
    // (confirmed by grep across phases_late.rs/net_phase_e.rs/tls.rs), which
    // made `getDefault()` above always return a fresh, unconfigured context
    // even after a caller called `setDefault` with a fully `.init()`'d one.
    // Static method: args[0] is the SSLContext parameter, no receiver.
    r.register(
        ctx_cls,
        "setDefault",
        "(Ljavax/net/ssl/SSLContext;)V",
        |_ctx, args| match args.first().copied() {
            Some(Value::Object(Some(ctx_obj))) => {
                crate::t27_tls::set_runtime_default_ssl_context(ctx_obj);
                Ok(None)
            }
            _ => Err(npe("context")),
        },
    );
    r.register(
        ctx_cls,
        "init",
        "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 1, Value::Int(1));
            // Per-SSLContext mTLS identity: claim the identity staged by the
            // keystore load that fed this context's KeyManager (same thread),
            // and attach it to this SSLContext. createSSLEngine / createSocket
            // then use THIS context's cert+key rather than the process-global
            // slot, so an in-process server and client don't clobber each other.
            crate::t27_tls::attach_pending_identity_to_ctx(ctx, this);
            // Stash the actual TrustManager objects passed here (may include a
            // revocation-aware PKIXRevocationChecker attached by
            // Tomcat's SSLUtilBase.getTrustManagers, or a fully custom
            // X509TrustManager). rustls's own verifier only checks the
            // certificate chain against a trust anchor — it never consults
            // these — so without this, custom/OCSP/CRL trust managers are
            // silently never invoked. Consulted post-handshake by
            // `t27_tls::engine_run_trust_check`.
            let tms_arr = match args.get(2) {
                Some(Value::Object(Some(a))) => Some(*a),
                _ => None,
            };
            crate::t27_tls::attach_trust_managers_to_ctx(ctx, this, tms_arr);
            // Stash the actual KeyManager objects too (may include a test
            // wrapper like Tomcat's `TrackingKeyManager`). rustls's own
            // client-cert path otherwise only ever presents one fixed
            // (cert_pem, key_pem) pair — it never consults a KeyManager's
            // `chooseClientAlias`, so a server-requested-issuer-specific
            // selection (or a wrapper that must observe the call) was
            // silently skipped. Consulted synchronously mid-handshake by
            // `t27_tls::JavaKeyManagerResolver` (`getSocketFactory()` reads
            // this back out via `ctx_key_managers_table`'s key).
            let kms_arr = match args.get(1) {
                Some(Value::Object(Some(a))) => Some(*a),
                _ => None,
            };
            crate::t27_tls::attach_key_managers_to_ctx(ctx, this, kms_arr);
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
            // getSocketFactory() is a client-side call (the server uses
            // createSSLEngine / getServerSocketFactory). Capture the complete
            // context for native HttpsURLConnection, including anonymous
            // client contexts: its ClientConfig owns the TLS ticket cache.
            // This is the reliable capture point because the real JDK
            // setDefaultSSLSocketFactory bytecode cannot be overridden here.
            crate::t27_tls::capture_huc_ssl_context(ctx, this);
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
    // getSupportedSSLParameters() — Tomcat's JSSEUtil.initialise() reads the
    // supported protocols + cipher suites here. The synthetic SSLContext has no
    // contextSpi, so the inherited javax bytecode NPEs; return a REAL
    // javax.net.ssl.SSLParameters (plain data holder) with the protocol/cipher
    // lists the rustls-backed engine negotiates.
    r.register(
        ctx_cls,
        "getSupportedSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, _args| {
            let protocols = ["TLSv1.3", "TLSv1.2"];
            let ciphers = [
                "TLS_AES_128_GCM_SHA256",
                "TLS_AES_256_GCM_SHA384",
                "TLS_CHACHA20_POLY1305_SHA256",
                "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
                "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
                "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
                "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
                "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
                "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
            ];
            let mk = |ctx: &mut dyn NativeContext, items: &[&str]| {
                let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, items.len());
                for (i, &s) in items.iter().enumerate() {
                    let so = ctx.create_string(s);
                    ctx.set_array_element(arr, i, Value::Object(Some(so)));
                }
                Value::Object(Some(arr))
            };
            let carr = mk(ctx, &ciphers);
            let parr = mk(ctx, &protocols);
            // SSLParameters(String[] cipherSuites, String[] protocols)
            ctx.new_object_initialized(
                "javax/net/ssl/SSLParameters",
                "([Ljava/lang/String;[Ljava/lang/String;)V",
                &[carr, parr],
            )
        },
    );
    // getDefaultSSLParameters() — BUG-08: Jetty's `SslContextFactory.load()`
    // (jetty-util) calls this on the SSLContext it just `getInstance`'d to seed
    // the connector's enabled protocols/cipher suites. Like
    // getSupportedSSLParameters above, the synthetic SSLContext carries no real
    // `contextSpi`, so the un-intercepted `javax.net.ssl.SSLContext`
    // .getDefaultSSLParameters() bytecode (`return contextSpi.engineGet…()`)
    // dereferenced null → NPE (5× org.springframework.http.client.
    // JettyClientHttpRequestFactoryTests). Return a REAL SSLParameters whose
    // *default-enabled* protocol/cipher lists match what the rustls-backed
    // engine negotiates — on JDK 25 the modern TLSv1.3/1.2 suites are
    // enabled-by-default, so the default set mirrors the supported set here.
    r.register(
        ctx_cls,
        "getDefaultSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, _args| {
            let protocols = ["TLSv1.3", "TLSv1.2"];
            let ciphers = [
                "TLS_AES_128_GCM_SHA256",
                "TLS_AES_256_GCM_SHA384",
                "TLS_CHACHA20_POLY1305_SHA256",
                "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
                "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
                "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
                "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
                "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
                "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
            ];
            let mk = |ctx: &mut dyn NativeContext, items: &[&str]| {
                let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, items.len());
                for (i, &s) in items.iter().enumerate() {
                    let so = ctx.create_string(s);
                    ctx.set_array_element(arr, i, Value::Object(Some(so)));
                }
                Value::Object(Some(arr))
            };
            let carr = mk(ctx, &ciphers);
            let parr = mk(ctx, &protocols);
            // SSLParameters(String[] cipherSuites, String[] protocols)
            ctx.new_object_initialized(
                "javax/net/ssl/SSLParameters",
                "([Ljava/lang/String;[Ljava/lang/String;)V",
                &[carr, parr],
            )
        },
    );
    // createSSLEngine() — return a rustls-backed sun.security.ssl.SSLEngineImpl
    // (its wrap/unwrap/handshake natives live in t27_tls::register_sslengine_real,
    // keyed by ObjectRef via engine_id_or_alloc, so a bare object suffices).
    for desc in [
        "()Ljavax/net/ssl/SSLEngine;",
        "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
    ] {
        r.register(ctx_cls, "createSSLEngine", desc, |ctx, args| {
            let eng = alloc_concurrent_synthetic(ctx, "sun/security/ssl/SSLEngineImpl", 4);
            // Copy this SSLContext's per-context identity (its keystore cert+key)
            // onto the engine, so the rustls handshake presents THIS context's
            // cert (server cert, or client cert for mTLS) instead of the global.
            if let Ok(sslctx) = obj_arg(args, 0) {
                let identity = crate::t27_tls::ctx_identity(ctx, sslctx);
                // A client context commonly has only trust material. Capture
                // its roots before the next context creation can replace the
                // thread-local selection used by the rustls engine.
                crate::t27_tls::set_engine_trust_roots_override(eng);
                if let Some((cert, key)) = identity {
                    crate::t27_tls::set_engine_identity_override(eng, cert, key);
                }
                // Remember which SSLContext created this engine so the
                // post-handshake trust check can find its TrustManager[]
                // (independent of whether a KMF identity was also present).
                crate::t27_tls::set_engine_trust_ctx_key(ctx, eng, sslctx);
            }
            Ok(Some(Value::Object(Some(eng))))
        });
    }
    // Netty's JdkSslClientContext reads this immediately after SSLContext.init.
    // Our bridged real-JDK SSLContext has no contextSpi, so the Java method
    // would otherwise dereference null despite the usable native TLS context.
    // The synthetic facade matches the server session-context contract below.
    r.register(
        ctx_cls,
        "getClientSessionContext",
        "()Ljavax/net/ssl/SSLSessionContext;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSessionContext", 0);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // getServerSessionContext() — Tomcat caches it and may set cache size /
    // timeout; return a synthetic SSLSessionContext (setters are no-ops).
    r.register(
        ctx_cls,
        "getServerSessionContext",
        "()Ljavax/net/ssl/SSLSessionContext;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSessionContext", 0);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    // SSLSessionContext setters Tomcat's SSLHostConfig drives — no-op (the
    // rustls engine manages its own session cache).
    let ssc = "javax/net/ssl/SSLSessionContext";
    r.register(ssc, "setSessionCacheSize", "(I)V", |_ctx, _args| Ok(None));
    r.register(ssc, "setSessionTimeout", "(I)V", |_ctx, _args| Ok(None));
    r.register(ssc, "getSessionCacheSize", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(ssc, "getSessionTimeout", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });

    let sf = "javax/net/ssl/SSLSocketFactory";
    r.register(
        sf,
        "createSocket",
        "(Ljava/lang/String;I)Ljava/net/Socket;",
        |ctx, args| {
            let this_factory = obj_arg(args, 0)?;
            let host_val = args.get(1).copied().unwrap_or(Value::Object(None));
            let host = value_or_string(ctx, host_val, "");
            let port = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            if host.is_empty() || !(1..=65535).contains(&port) {
                return Err(iae(format!("bad host/port: {host}:{port}")));
            }
            // Per-context client identity (mTLS): the SSLContext stashed on the
            // factory by getSocketFactory (field 0) may carry a client cert+key.
            //
            // FIX (client-cipher-restriction): field 0 is only the owning
            // SSLContext for the bare placeholder `getSocketFactory()`
            // itself returns. When this `createSocket` is reached via an
            // up-call on a real, user-defined `SSLSocketFactory` SUBCLASS
            // (e.g. Tomcat's `TesterSupport.ClientSSLSocketFactory`, wrapping
            // the placeholder — see `http_url_connection
            // ::huc_upcall_create_socket_if_custom_factory`), field 0 is
            // whatever THAT class declares first (for `ClientSSLSocketFactory`,
            // its own `delegate` field, a *different* SSLSocketFactory
            // object) — `ctx_identity` then looks up identity keyed by the
            // wrong object's identity and always misses, so the up-called
            // path silently dropped client-cert presentation entirely. Fall
            // back to the same global `huc_default_client_identity()` that
            // `http_url_connection::perform`'s own (non-up-called) connect
            // already relies on — populated by the same `getSocketFactory()`
            // call, just read through a path that isn't sensitive to which
            // object ends up at field 0.
            let client_ident = match ctx.get_field(this_factory, 0) {
                Value::Object(Some(sslctx)) => crate::t27_tls::ctx_identity(ctx, sslctx),
                _ => None,
            }
            .or_else(crate::t27_tls::huc_default_client_identity);
            #[cfg(unix)]
            let legacy_dsa_client = client_ident
                .as_ref()
                .is_some_and(|(_, key_pem)| crate::t27_tls::is_dsa_private_key_pem(key_pem));
            #[cfg(unix)]
            let legacy_dsa_roots = crate::t27_tls::selected_context_trust_root_ders();
            // Use the rustls client path rather than a default native-tls
            // connector: (1) trust the gathered test/truststore roots (the
            // native-tls default trusts only the OS root store, so it cannot
            // validate a test CA — which broke every loopback HTTPS client),
            // and (2) present the client certificate for mTLS when present.
            // `km_ctx_key: None` here — this `SSLSocketFactory.createSocket`
            // path (`rustls_client_connect`'s own handshake loop) doesn't yet
            // publish an active native context around its handshake, so it
            // keeps presenting the fixed pre-captured identity rather than
            // consulting a live KeyManager. Only `http_url_connection::
            // perform` (the path `HttpsURLConnection`/`TestClientCert` uses)
            // does the latter today — see
            // docs/internal/fixed-suite-bugs/tls-ocsp-clientcert-validation-not-enforced-FIXED.md.
            let cfg = crate::t27_tls::build_engine_client_config_with_identity(
                &["http/1.1"],
                client_ident.as_ref().map(|(c, k)| (c.as_str(), k.as_str())),
                None,
                None,
            )
            .map_err(|e| ioex(format!("client TLS config: {e}")))?;
            // T19.H1: TCP connect + full TLS handshake blocks for real, and this
            // native can now be reached via an up-call from
            // `http_url_connection::huc_upcall_create_socket_if_custom_factory`
            // that runs BEFORE the caller's own blocking region (it needs `ctx`,
            // which a blocking region must not hold) — without announcing our
            // own blocking region here, a concurrent stop-the-world GC would wait
            // forever for this thread to reach a safepoint it can't reach until
            // the (now-deadlocked-behind-the-GC) network call returns.
            ctx.begin_blocking_region();
            #[cfg(unix)]
            let connect_result = if legacy_dsa_client {
                crate::servlet::s2_legacy_dsa_tls_connect(&host, port as u16, &legacy_dsa_roots)
            } else {
                crate::t27_tls::rustls_client_connect(cfg, &host, port as u16)
                    .map(|rid| crate::servlet::RUSTLS_SOCK_ID_BASE + rid)
            };
            #[cfg(not(unix))]
            let connect_result = crate::t27_tls::rustls_client_connect(cfg, &host, port as u16)
                .map(|rid| crate::servlet::RUSTLS_SOCK_ID_BASE + rid);
            ctx.end_blocking_region();
            let id = connect_result.map_err(|e| ioex(format!("TLS connect: {e}")))?;
            let sock = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocket", 5);
            let pin_base = ctx.pin_native_root(sock);
            let host_s = ctx.create_string(&host);
            let sock = ctx.read_native_pin(pin_base, sock);
            ctx.set_field(sock, SOCK_HOST, Value::Object(Some(host_s)));
            sock_set(ctx, sock, |s| {
                s.port = port;
                s.local_port = 0;
                s.closed = 0;
                s.stream_id = id;
            });
            let sock = ctx.read_native_pin(pin_base, sock);
            ctx.unpin_native_roots(pin_base);
            if std::env::var_os("CRATONVM_DBG_TLS_SOCK").is_some() {
                eprintln!(
                    "[dbg-tls-sock] thread={:?} net_phase_e createSocket(String,int) built sock={:?} stream_id={}",
                    std::thread::current().id(),
                    sock,
                    id
                );
            }
            Ok(Some(Value::Object(Some(sock))))
        },
    );
    // FIX (client-cipher-restriction): a socket from `createSocket` above
    // already completed its handshake unrestricted. The JDK contract
    // callers rely on (e.g. Tomcat's `TesterSupport.ClientSSLSocketFactory`,
    // which calls this immediately after `createSocket` returns, before any
    // I/O) is that restricting to a suite the server doesn't support makes
    // the connection fail — so the only way to honor it is to tear down and
    // reconnect under the restriction.
    //
    // Only do this when at least one requested suite maps to a real rustls
    // `CipherSuite` (`any_cipher_mappable`) — classic TLS 1.2 `TLS_DHE_RSA_*`
    // names never do, because rustls has no finite-field DHE support in any
    // crypto provider (a real upstream limitation, not a gap in this
    // mapping). For an unmappable list, leave the existing (unrestricted)
    // connection alone rather than reconnect into a config that would
    // silently fall back to unrestricted anyway (see `cipher_provider_for`'s
    // own empty-`wanted` fallback) — that would tear down a working
    // connection for no enforcement benefit. Known, currently unclosable gap
    // for TLS 1.2 DHE-suite restriction specifically; see
    // docs/internal/fixed-suite-bugs/tls-ocsp-clientcert-validation-not-enforced-FIXED.md.
    r.register(
        "javax/net/ssl/SSLSocket",
        "setEnabledCipherSuites",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mut ciphers: Vec<String> = Vec::new();
            if let Some(Value::Object(Some(arr))) = args.get(1) {
                let len = ctx.array_length(*arr);
                for i in 0..len {
                    if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                        if let Some(t) = ctx.read_string(s) {
                            ciphers.push(t);
                        }
                    }
                }
            }
            if ciphers.is_empty() || !crate::t27_tls::any_cipher_mappable(&ciphers) {
                return Ok(None);
            }
            // FIX (netty-client-socket-write-after-close residual): don't
            // reconnect when `ciphers` covers this socket's ENTIRE supported
            // set (`ssl_sock_supported_cipher_suites` in phases_late.rs, the
            // list `SSLSocket.getSupportedCipherSuites()` returns) — that's
            // not a real restriction, just a caller re-asserting "use
            // everything you support" (the common case: e.g. Apache
            // HttpClient5's `SSLConnectionSocketFactory` calls
            // `setEnabledCipherSuites(getSupportedCipherSuites())` as part of
            // its normal connection setup, unconditionally, for every socket
            // it creates — not only ones under an actual cipher policy).
            // Reconnecting anyway tears down a connection that may have been
            // established under semantics THIS rustls-only path cannot
            // reproduce (a Java TrustManager accepting a self-signed/test
            // certificate — see `new13_do_create_socket`'s post-connect
            // `checkServerTrusted` delegation, which this reconnect has no
            // way to redo), turning a harmless no-op call into a hard
            // failure. Only reconnect for a GENUINE restriction: a proper
            // subset of the supported suites (e.g. Tomcat's
            // `TesterSupport.ClientSSLSocketFactory`, which this reconnect
            // exists for in the first place — see the FIX comment above).
            let requested: std::collections::HashSet<&str> =
                ciphers.iter().map(String::as_str).collect();
            let is_full_supported_set = CLIENT_SUPPORTED_CIPHER_SUITES
                .iter()
                .all(|c| requested.contains(c));
            if is_full_supported_set {
                return Ok(None);
            }
            let host = read_field_string_or(ctx, this, SOCK_HOST, "");
            let side = sock_get(ctx, this);
            if host.is_empty() || side.port <= 0 {
                return Ok(None);
            }
            let client_ident = crate::t27_tls::huc_default_client_identity();
            let cfg = match crate::t27_tls::build_engine_client_config_with_identity_ciphers(
                &["http/1.1"],
                client_ident.as_ref().map(|(c, k)| (c.as_str(), k.as_str())),
                &ciphers,
            ) {
                Ok(cfg) => cfg,
                Err(msg) => {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "javax/net/ssl/SSLHandshakeException",
                        &msg,
                    ));
                }
            };
            // T19.H1: see the matching comment on `createSocket` above — this
            // reconnect blocks on real network I/O too and must announce it.
            ctx.begin_blocking_region();
            let connect_result =
                crate::t27_tls::rustls_client_connect(cfg, &host, side.port as u16);
            ctx.end_blocking_region();
            match connect_result {
                Ok(rid) => {
                    if side.stream_id >= 0 {
                        let _ = crate::servlet::s2_tls_close(side.stream_id);
                    }
                    let new_id = crate::servlet::RUSTLS_SOCK_ID_BASE + rid;
                    sock_set(ctx, this, |s| {
                        s.stream_id = new_id;
                    });
                    Ok(None)
                }
                Err(msg) => Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "javax/net/ssl/SSLHandshakeException",
                    &msg,
                )),
            }
        },
    );
    r.register(
        sf,
        "getDefault",
        "()Ljavax/net/SocketFactory;",
        |ctx, _args| {
            let f = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 1);
            ctx.set_field(f, 0, Value::Object(None));
            Ok(Some(Value::Object(Some(f))))
        },
    );
}

// ===========================================================================
// RE.7 — java.net.DatagramSocket
// ===========================================================================

// DatagramSocket state now lives in the `ds_side_table()` (see `DsSide`),
// keyed by ObjectRef — the old DS_PORT/DS_CLOSED/DS_TIMEOUT/DS_FD object-slot
// layout collided with the real-JDK single-field `DatagramSocket`.

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
        ds_set(this, |s| {
            s.port = port;
            s.closed = 0;
            s.timeout = 0;
            s.fd = fd as i32;
        });
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
        ds_set(this, |s| {
            s.port = actual_port;
            s.closed = 0;
            s.timeout = 0;
            s.fd = fd as i32;
        });
        Ok(None)
    });
    r.register(ds, "<init>", "(ILjava/net/InetAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let host = match args.get(2) {
            Some(Value::Object(Some(ia))) => {
                inet_addr_field_string_or(ctx, *ia, IA_ADDR, "0.0.0.0")
            }
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
        ds_set(this, |s| {
            s.port = actual_port;
            s.closed = 0;
            s.timeout = 0;
            s.fd = fd as i32;
        });
        Ok(None)
    });

    r.register(ds, "send", "(Ljava/net/DatagramPacket;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pkt = obj_arg(args, 1)?;
        let fd = ds_get(this).fd;
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
            Value::Object(Some(ia)) => inet_addr_field_string_or(ctx, ia, IA_ADDR, ""),
            _ => String::new(),
        };
        if host.is_empty() || !(1..=65535).contains(&port) {
            return Err(iae(format!("DatagramPacket: bad addr {host}:{port}")));
        }
        let payload = java_byte_array_to_vec(ctx, data_arr, 0, len)?;
        let target = format!("{host}:{port}");
        // Bracket the send in the GC-blocking protocol: it can park on a full
        // local socket buffer, same rationale as MulticastSocket's send in
        // native-io/src/net.rs — without this a cross-thread STW GC would
        // wait for the syscall to return instead of the thread reaching a
        // safepoint. No heap refs are read after the call, so a plain
        // begin/end pair (no ref re-sync) suffices.
        ctx.begin_blocking_region();
        let send_result = ctx.fd_table().udp_send(fd as u32, &payload, &target);
        ctx.end_blocking_region();
        send_result.map_err(|e| ioex(format!("UDP send: {e}")))?;
        Ok(None)
    });

    r.register(
        ds,
        "receive",
        "(Ljava/net/DatagramPacket;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pkt = obj_arg(args, 1)?;
            let fd = ds_get(this).fd;
            if fd < 0 {
                return Err(ioex("DatagramSocket: closed"));
            }
            let data_arr = match ctx.get_field(pkt, DP_DATA) {
                Value::Object(Some(a)) => a,
                _ => return Err(ioex("DatagramPacket: null data")),
            };
            let cap = ctx.array_length(data_arr);
            let mut buf = vec![0u8; cap];
            let timeout_ms = ds_get(this).timeout;
            let d = if timeout_ms > 0 {
                Some(Duration::from_millis(timeout_ms as u64))
            } else {
                None
            };
            ctx.fd_table()
                .udp_set_read_timeout(fd as u32, d)
                .map_err(|e| ioex(format!("UDP timeout: {e}")))?;
            // GC-blocking audit: this recv parks in the OS for up to
            // soTimeout — or indefinitely when no timeout is set (same hang
            // mechanism documented on MulticastSocket's receive in
            // native-io/src/net.rs, which this DatagramSocket-keyed
            // registration duplicates for callers resolved via the
            // DatagramSocket-declaring class). Without the blocking-region
            // bracket, a cross-thread STW GC requested while this thread is
            // parked in udp_recv can never be satisfied: the thread isn't in
            // JIT code (can't be taken over) and isn't at a safepoint (can't
            // cooperate), so `pending` never reaches 0 and the takeover loop
            // spins until the external harness timeout. `pkt`/`data_arr` are
            // re-synced afterward in case a moving GC ran while parked.
            let mut blocked_refs = [Value::Object(Some(pkt)), Value::Object(Some(data_arr))];
            ctx.begin_blocking_region();
            let recv_result = ctx.fd_table().udp_recv(fd as u32, &mut buf);
            ctx.end_blocking_region_refs(&mut blocked_refs);
            let pkt = match blocked_refs[0] {
                Value::Object(Some(o)) => o,
                _ => pkt,
            };
            let data_arr = match blocked_refs[1] {
                Value::Object(Some(o)) => o,
                _ => data_arr,
            };
            let (n, origin) = recv_result.map_err(|e| ioex(format!("UDP recv: {e}")))?;
            copy_bytes_into_java_array(ctx, data_arr, 0, &buf[..n])?;
            ctx.set_field(pkt, DP_LENGTH, Value::Int(n as i32));
            if let Some((oh, op)) = origin.rsplit_once(':') {
                let port = op.parse::<i32>().unwrap_or(0);
                let ia = alloc_inet_address(ctx, oh, oh);
                ctx.set_field(pkt, DP_ADDR, Value::Object(Some(ia)));
                ctx.set_field(pkt, DP_PORT, Value::Int(port));
            }
            Ok(None)
        },
    );

    r.register(ds, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd = ds_get(this).fd;
        if fd >= 0 {
            // Ignore close errors — the fd may already be closed by
            // a racing caller; `closed` is still set unconditionally.
            let _ = ctx.fd_table().close(fd as u32);
        }
        ds_set(this, |s| {
            s.closed = 1;
            s.fd = -1;
        });
        Ok(None)
    });
    r.register(ds, "isClosed", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ds_get(this).closed)))
    });
    r.register(ds, "getLocalPort", "()I", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ds_get(this).port)))
    });
    r.register(ds, "setSoTimeout", "(I)V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if ms < 0 {
            return Err(iae("negative SO_TIMEOUT"));
        }
        ds_set(this, |s| s.timeout = ms);
        Ok(None)
    });
    r.register(ds, "getSoTimeout", "()I", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ds_get(this).timeout)))
    });

    // Same rationale as the `ServerSocket` setReuseAddress no-op: the synthetic
    // `DatagramSocket` has no real impl, so the JDK `setReuseAddress` bytecode
    // would NPE on uninitialised socket state. WildFly's `isPortAvailable` calls
    // `new DatagramSocket(port); setReuseAddress(true)` right after the
    // ServerSocket check.
    r.register(ds, "setReuseAddress", "(Z)V", |_ctx, _args| Ok(None));
    r.register(ds, "getReuseAddress", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
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
        let name = ctx.create_string(if matches!(ip, IpAddr::V4(_)) {
            "eth0"
        } else {
            "eth1"
        });
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

    // `NetworkInterface.<clinit>` calls the JNI library initializer
    // `init()V` — unregistered it surfaced as UnsatisfiedLinkError and
    // killed any class-init touching NetworkInterface (Gradle's user-home
    // services during ProjectBuilder bootstrap). The real init only caches
    // JNI field IDs; a no-op is faithful.
    r.register(ni, "init", "()V", |_ctx, _args| Ok(None));

    r.register(ni, "getHardwareAddress", "()[B", |ctx, _args| {
        let mac = ctx.new_array(ArrayElementType::Byte, 6);
        for i in 0..6 {
            ctx.set_array_element(mac, i, Value::Int(0));
        }
        Ok(Some(Value::Object(Some(mac))))
    });
    r.register(ni, "getMTU", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(1500)))
    });

    // Low-level "0" suffixed natives used by NetworkInterface (JDK internals).
    // Safe no-op defaults — sufficient for environment probing (e.g., Spring's
    // HostInfoEnvironmentPostProcessor) without performing real OS queries.
    r.register(ni, "isUp0", "(Ljava/lang/String;I)Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    r.register(ni, "isLoopback0", "(Ljava/lang/String;I)Z", |ctx, args| {
        let name = match args.first().copied() {
            Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let index = match args.get(1).copied() {
            Some(Value::Int(i)) => i,
            _ => 0,
        };
        Ok(Some(Value::Int(if name == "lo" || index == 1 {
            1
        } else {
            0
        })))
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
            // One REAL-layout loopback interface, built via the
            // package-private NetworkInterface(String,int,InetAddress[])
            // constructor so the real getInetAddresses()/toString bytecode
            // reads the right fields (a synthetic 5-slot object breaks them
            // — see the note above). An empty array here made
            // getNetworkInterfaces() throw SocketException("No network
            // interfaces configured"); most callers fall back, but Gradle's
            // InetAddressFactory turns it into "Could not determine a usable
            // wildcard IP for this machine" and every user-home-scope
            // service dies (ProjectBuilder bootstrap).
            let empty = |ctx: &mut dyn NativeContext| {
                let arr = ctx.new_ref_array(ClassId::new(0), 0);
                Ok(Some(Value::Object(Some(arr))))
            };
            let lo_addr = match ctx.invoke(
                "java/net/InetAddress",
                "getLoopbackAddress",
                "()Ljava/net/InetAddress;",
                &[],
            ) {
                Ok(Some(Value::Object(Some(a)))) => a,
                _ => return empty(ctx),
            };
            let lo_pin = ctx.pin_native_root(lo_addr);
            let addr_cid = ctx
                .class_id_by_name("java/net/InetAddress")
                .unwrap_or(ClassId::new(0));
            let addrs = ctx.new_ref_array(addr_cid, 1);
            let lo_addr = ctx.read_native_pin(lo_pin, lo_addr);
            ctx.set_array_element(addrs, 0, Value::Object(Some(lo_addr)));
            let addrs_pin = ctx.pin_native_root(addrs);
            let name = ctx.create_string("lo");
            let name_pin = ctx.pin_native_root(name);
            let iface = match ctx.new_object("java/net/NetworkInterface") {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => {
                    ctx.unpin_native_roots(lo_pin);
                    ctx.unpin_native_roots(addrs_pin);
                    ctx.unpin_native_roots(name_pin);
                    return empty(ctx);
                }
            };
            let iface_pin = ctx.pin_native_root(iface);
            let name = ctx.read_native_pin(name_pin, name);
            let addrs = ctx.read_native_pin(addrs_pin, addrs);
            let _ = ctx.invoke(
                "java/net/NetworkInterface",
                "<init>",
                "(Ljava/lang/String;I[Ljava/net/InetAddress;)V",
                &[
                    Value::Object(Some(iface)),
                    Value::Object(Some(name)),
                    Value::Int(1),
                    Value::Object(Some(addrs)),
                ],
            );
            let iface = ctx.read_native_pin(iface_pin, iface);
            let ni_cid = ctx
                .class_id_by_name("java/net/NetworkInterface")
                .unwrap_or(ClassId::new(0));
            // The package-private `(String,int,InetAddress[])` ctor leaves the
            // `childs` field null — the real JDK's native `getAll0` is what
            // populates it. `NetworkInterface.getSubInterfaces()` returns an
            // anonymous Enumeration whose `hasMoreElements()` reads
            // `childs.length`, so a null `childs` throws
            // `NullPointerException: arraylength null` (NetworkInterface$1).
            // That kills `NetworkUtils.<clinit>` (its `addAllInterfaces`
            // recursion calls `Collections.list(intf.getSubInterfaces())`) with
            // an ExceptionInInitializerError in every Elasticsearch ESTestCase
            // that touches networking. Set an empty `NetworkInterface[]` so
            // sub-interface enumeration yields zero elements (a loopback
            // interface has no sub-interfaces). Done before allocating `arr` so
            // `empty_childs` stays GC-rooted via `iface.childs`.
            let empty_childs = ctx.new_ref_array(ni_cid, 0);
            let iface = ctx.read_native_pin(iface_pin, iface);
            ctx.set_field_by_name(iface, "childs", Value::Object(Some(empty_childs)));
            let arr = ctx.new_ref_array(ni_cid, 1);
            let iface = ctx.read_native_pin(iface_pin, iface);
            ctx.set_array_element(arr, 0, Value::Object(Some(iface)));
            ctx.unpin_native_roots(lo_pin);
            ctx.unpin_native_roots(addrs_pin);
            ctx.unpin_native_roots(name_pin);
            ctx.unpin_native_roots(iface_pin);
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
                if let Ok(Some(Value::Int(n))) = ctx.invoke_virtual(this, "selectNow", "()I", &[]) {
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
    // Behind a Mutex so stop() can `take()` (and thus close) the OS listener
    // SYNCHRONOUSLY — a stopped host must immediately refuse connections so a
    // round-robin client retries another node instead of connecting into a dead
    // server (ES MultipleHosts stopRandomHost). The accept loop locks it briefly
    // per (non-blocking) accept; taking it makes the next iteration exit.
    listener: Mutex<Option<TcpListener>>,
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

/// GC root scan for the synthetic `com.sun.net.httpserver` server registry.
///
/// Each registered `HttpHandlerEntry.handler` is a live Java `HttpHandler`
/// ObjectRef stored OUTSIDE the Java heap (in this native `server_registry`),
/// so the collector has no other view of it. Without rooting it here a moving
/// young GC could either reclaim the handler (the only strong reference is this
/// native map) or relocate it and leave the stored ObjectRef dangling — and the
/// per-request dispatcher (`re10_dispatch_pending`) then `invoke_virtual`s a
/// stale receiver, which resolves to `java/lang/Object` and fails with
/// `NoSuchMethodError: java/lang/Object.handle(...)`. Observed as an intermittent
/// storm under `-Xmx1g` GC pressure (ES `RestClientSingleHostIntegTests`
/// `testManyAsyncRequests`), GC-frequency-dependent (green at large heaps). The
/// companion `gc_update_re10_handler_refs` re-points the stored refs after a
/// move; this scan keeps them live across the collection.
///
/// Mirrors the established native-root pattern (locale / logmanager / jboss_msc).
/// Arcs are cloned out from under the registry lock first so the registry lock
/// and the per-server `handlers` lock are never held simultaneously.
pub fn gc_scan_re10_handler_roots(out: &mut Vec<ObjectRef>) {
    let states: Vec<std::sync::Arc<ServerState>> = {
        let reg = server_registry().lock();
        reg.values().cloned().collect()
    };
    for state in states {
        let hs = state.handlers.lock();
        for e in hs.iter() {
            out.push(e.handler);
        }
    }
}

/// Companion to [`gc_scan_re10_handler_roots`]: after a moving collection,
/// re-point every stored `HttpHandlerEntry.handler` to its relocated address so
/// the dispatcher invokes the live handler, not a vacated from-space slot. A
/// no-op when nothing moved (empty `pointer_map`) or for handlers the collector
/// left in place (absent from the map).
pub fn gc_update_re10_handler_refs(pointer_map: &std::collections::HashMap<usize, usize>) {
    if pointer_map.is_empty() {
        return;
    }
    let states: Vec<std::sync::Arc<ServerState>> = {
        let reg = server_registry().lock();
        reg.values().cloned().collect()
    };
    for state in states {
        let mut hs = state.handlers.lock();
        for e in hs.iter_mut() {
            let old = e.handler.as_ptr() as usize;
            if let Some(&new) = pointer_map.get(&old) {
                debug_assert!(new != 0, "GC pointer map contains null address");
                // SAFETY: `new` is a relocated address produced by the collector's
                // pointer map for this exact object; it points at a valid header.
                e.handler = unsafe { ObjectRef::from_raw(new as *mut u8) };
            }
        }
    }
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

/// VULN-FIX [nb-net-phase-e]: configurable upper bound on the size of an inbound
/// HTTP request body for the embedded com.sun.net.httpserver. Previously
/// `parse_http_request` read up to the client-supplied `Content-Length` with NO
/// upper bound, allowing a remote peer to exhaust process memory (DoS) by
/// advertising (and streaming) an enormous body. We now cap the accepted body
/// and reject anything larger with a 413 response.
///
/// The cap is read once from `CRATONVM_HTTP_MAX_BODY` (bytes); it defaults to
/// 8 MiB, which is comfortably larger than any normal request the test suite
/// issues while still bounding worst-case allocation. A value of 0 or an
/// unparseable value falls back to the default.
fn http_max_request_body() -> usize {
    static MAX: OnceLock<usize> = OnceLock::new();
    *MAX.get_or_init(|| {
        const DEFAULT: usize = 8 * 1024 * 1024; // 8 MiB
        std::env::var("CRATONVM_HTTP_MAX_BODY")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT)
    })
}

/// VULN-FIX [nb-net-phase-e]: best-effort write of a fixed minimal HTTP response
/// to a peer we are about to reject (e.g. 413 for an oversized body, 400 for a
/// malformed/duplicate Content-Length). Used so that the parse path can refuse a
/// request cheaply without allocating its body and still tell the client why.
fn http_reject_and_close(stream: &mut TcpStream, status: i32) {
    let body = http_reason(status);
    let mut resp = Vec::with_capacity(96 + body.len());
    let _ = write!(
        &mut resp,
        "HTTP/1.1 {status} {body}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(&resp);
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

fn parse_http_request(mut stream: TcpStream) -> Option<PendingRequest> {
    // The accept loop sets the LISTENER non-blocking; on Windows the accepted
    // stream inherits that mode, so a bare `read` returns WouldBlock the instant
    // the peer hasn't sent yet — which `Err(_) => return None` below would treat
    // as a dead connection and drop the socket. A well-behaved client that
    // connects slightly before it writes its request (e.g. the Apache NIO
    // reactor, which establishes the connection then writes on the next event
    // loop turn) would then see an immediate EOF / "Connection is closed". Force
    // the accepted stream BLOCKING so the read timeout below actually governs and
    // we wait for the request.
    stream.set_nonblocking(false).ok();
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 1024];
    // PERF [nb-net-phase-e]: scan only the newly-appended bytes for the
    // "\r\n\r\n" header terminator instead of re-running `buf.windows(4)` over
    // the whole accumulated buffer on every ~1 KiB read. Re-scanning from 0 each
    // read is O(n^2) in header size. `scanned` records how many leading bytes
    // have already been checked; each read we restart the window scan 3 bytes
    // before that point so a terminator straddling the previous/new boundary is
    // still detected, then capture its absolute position so the post-loop step
    // need not re-scan either. The detection result is identical to the original
    // full-buffer `windows(4).position(...)`.
    let mut scanned = 0usize;
    let mut sep: Option<usize> = None;
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                // Fast-reject non-HTTP traffic: every HTTP request line starts
                // with an uppercase-ASCII method token (GET/POST/PUT/...). A TLS
                // ClientHello (0x16 ...) or other binary garbage never will, so
                // bail immediately instead of blocking the (now-blocking) read
                // until the timeout. Without this, an HTTPS client against the
                // plaintext synthetic server (com.sun.net.httpserver.HttpsServer
                // is not TLS-capable here) would hang the whole suite waiting for
                // a ServerHello that never comes — RestClientBuilderIntegTests.
                if !buf.is_empty() && !buf[0].is_ascii_uppercase() {
                    return None;
                }
                // Restart 3 bytes before the previously-scanned end so a
                // terminator split across the boundary is not missed (saturating
                // so the first read starts at 0).
                let start = scanned.saturating_sub(3);
                if let Some(rel) = buf[start..].windows(4).position(|w| w == b"\r\n\r\n") {
                    sep = Some(start + rel);
                    break;
                }
                // Everything except the trailing 3 bytes (which may begin a
                // boundary-straddling terminator) is now fully checked.
                scanned = buf.len().saturating_sub(3);
                if buf.len() > 1 << 20 {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    let sep = sep?;
    let head = std::str::from_utf8(&buf[..sep]).ok()?;
    let mut lines = head.split("\r\n");
    let req_line = lines.next()?;
    let mut rl = req_line.splitn(3, ' ');
    let method = rl.next()?.to_string();
    let uri = rl.next()?.to_string();
    let _ = rl.next()?;
    let mut headers = Vec::new();
    // VULN-FIX [nb-net-phase-e]: track Content-Length as an Option and DETECT
    // duplicate/conflicting declarations instead of silently letting the last
    // one win. Two differing Content-Length values (or a malformed one) are a
    // classic request-smuggling vector, so we reject the request with 400.
    let mut content_length: Option<usize> = None;
    // VULN-FIX [nb-net-phase-e]: collect the full Transfer-Encoding declaration.
    // Previously this parser framed the body STRICTLY by Content-Length and never
    // inspected Transfer-Encoding, so a peer sending `Transfer-Encoding: chunked`
    // (with no Content-Length, or with a lying one) got content_length=0: we read
    // ZERO body bytes and left the entire chunked payload unread in the socket
    // buffer. On a keep-alive connection the next request then mis-parses that
    // leftover payload — classic request-smuggling / body desync (RFC 7230
    // §3.3.3). We now accumulate every transfer coding (a single value may be a
    // comma list, and multiple header lines are equivalent to one comma-joined
    // list per RFC 7230 §3.2.2) and resolve the framing after the loop.
    let mut transfer_codings: Vec<String> = Vec::new();
    for line in lines {
        if let Some(colon) = line.find(':') {
            let k = line[..colon].trim().to_string();
            let v = line[colon + 1..].trim().to_string();
            if k.eq_ignore_ascii_case("transfer-encoding") {
                for coding in v.split(',') {
                    let c = coding.trim();
                    if !c.is_empty() {
                        transfer_codings.push(c.to_ascii_lowercase());
                    }
                }
            }
            if k.eq_ignore_ascii_case("content-length") {
                // A header value may itself be a comma-separated list of equal
                // values (RFC 9110 §8.6); any unparseable or conflicting value
                // is treated as malformed.
                let parsed = v
                    .split(',')
                    .map(|p| p.trim())
                    .try_fold(None::<usize>, |acc, p| {
                        let n: usize = p.parse().ok()?;
                        match acc {
                            Some(prev) if prev != n => None, // conflicting list members
                            _ => Some(Some(n)),
                        }
                    })
                    .flatten();
                match parsed {
                    Some(n) => match content_length {
                        Some(prev) if prev != n => {
                            // Conflicting duplicate Content-Length headers.
                            http_reject_and_close(&mut stream, 400);
                            return None;
                        }
                        _ => content_length = Some(n),
                    },
                    None => {
                        // Unparseable / list with differing values.
                        http_reject_and_close(&mut stream, 400);
                        return None;
                    }
                }
            }
            headers.push((k, v));
        }
    }
    let max_body = http_max_request_body();
    // VULN-FIX [nb-net-phase-e]: resolve message framing per RFC 7230 §3.3.3.
    // Transfer-Encoding, when present, takes precedence over Content-Length and
    // determines how the body is delimited. Handle it BEFORE the Content-Length
    // path below.
    if !transfer_codings.is_empty() {
        // RFC 7230 §3.3.1: a sender MUST NOT apply chunked more than once, and
        // for a request to be framed by chunked it must be the FINAL coding.
        // We only support `chunked` (optionally as the sole/last coding) and the
        // no-op `identity`; anything else (gzip/deflate/compress, or chunked not
        // last) is something we cannot safely de-frame, so we reject rather than
        // guess at the body boundary (a guess is exactly the smuggling hazard).
        let last_is_chunked = transfer_codings.last().map(|c| c == "chunked") == Some(true);
        let chunked_count = transfer_codings.iter().filter(|c| *c == "chunked").count();
        let only_identity_or_chunked = transfer_codings
            .iter()
            .all(|c| c == "chunked" || c == "identity");
        if !last_is_chunked || chunked_count != 1 || !only_identity_or_chunked {
            // Unknown/unsupported transfer coding, or chunked applied more than
            // once / not last — reject instead of mis-framing the body.
            http_reject_and_close(&mut stream, 400);
            return None;
        }
        // RFC 7230 §3.3.3 (3): if a message is received with BOTH a
        // Transfer-Encoding and a Content-Length, the Content-Length MUST be
        // treated as suspect — a strong signal of request smuggling. Reject
        // outright rather than trusting either framing.
        if content_length.is_some() {
            http_reject_and_close(&mut stream, 400);
            return None;
        }
        // The chunked body may not have fully arrived with the header (we read
        // in ~1 KiB blocks above). Keep reading until the terminating zero-size
        // chunk `0\r\n\r\n` is present, bounding the accumulated raw size at the
        // cap so a peer can't stream unbounded data (chunked has no advertised
        // length) and exhaust memory. The cap also covers the inter-chunk
        // framing overhead, which is acceptable for a defensive upper bound.
        let mut raw = buf[sep + 4..].to_vec();
        if raw.len() > max_body {
            http_reject_and_close(&mut stream, 413);
            return None;
        }
        // Completeness is decided by actually walking the chunk framing with the
        // shared decoder rather than by scanning for a `0\r\n\r\n` byte pattern:
        // that pattern can legitimately occur INSIDE chunk data, which would
        // truncate the body early. `http_decode_chunked` returns `Ok` only once
        // the full framing (through the terminating zero chunk) is present, so we
        // read more whenever it still errors — until we succeed, the peer closes,
        // the read times out, or we hit the byte cap.
        let body = loop {
            match http_decode_chunked(&raw) {
                Ok(b) => break b,
                Err(_) => match stream.read(&mut tmp) {
                    Ok(0) => {
                        // Peer closed before a complete, valid chunked body.
                        http_reject_and_close(&mut stream, 400);
                        return None;
                    }
                    Ok(n) => {
                        raw.extend_from_slice(&tmp[..n]);
                        if raw.len() > max_body {
                            http_reject_and_close(&mut stream, 413);
                            return None;
                        }
                    }
                    Err(_) => {
                        // Read timeout / I/O error with an incomplete body.
                        http_reject_and_close(&mut stream, 400);
                        return None;
                    }
                },
            }
        };
        // The decoded payload must itself stay within the cap (the raw cap bounds
        // framing + data, but enforce on the decoded size too, defensively).
        if body.len() > max_body {
            http_reject_and_close(&mut stream, 413);
            return None;
        }
        return Some(PendingRequest {
            stream,
            method,
            uri,
            headers,
            body,
        });
    }
    let content_length = content_length.unwrap_or(0);
    // VULN-FIX [nb-net-phase-e]: bound the advertised body length BEFORE we read
    // or allocate anything. Without this a remote peer could send a huge
    // Content-Length and stream gigabytes, exhausting process memory.
    if content_length > max_body {
        http_reject_and_close(&mut stream, 413);
        return None;
    }
    // Any bytes already pulled in while reading the header also count toward the
    // bounded body. Clamp the initial slice so a peer can't smuggle past the cap
    // via a body that arrived in the same read as the header terminator.
    let leftover = &buf[sep + 4..];
    if leftover.len() > max_body {
        http_reject_and_close(&mut stream, 413);
        return None;
    }
    // Allocate with bounded capacity (never the unbounded client value): we will
    // read at most `content_length` bytes, itself already <= max_body.
    let mut body = Vec::with_capacity(content_length.min(max_body));
    body.extend_from_slice(leftover);
    while body.len() < content_length {
        // Read no more than what is still wanted; stop hard at the cap.
        let want = content_length - body.len();
        let chunk = want.min(tmp.len());
        match stream.read(&mut tmp[..chunk]) {
            Ok(0) => break,
            Ok(n) => {
                body.extend_from_slice(&tmp[..n]);
                if body.len() > max_body {
                    // Defensive: should be unreachable given the checks above,
                    // but never let the buffer grow past the cap.
                    http_reject_and_close(&mut stream, 413);
                    return None;
                }
            }
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

/// Build a real `com.sun.net.httpserver.Headers` (a `HashMap<String,List<String>>`
/// subclass) and populate it via its real `add` bytecode, so the Java
/// `HttpHandler` sees a fully-functional Map (`entrySet`/`get`/`getFirst` all
/// work). Verified byte-identical to HotSpot incl. header-name normalisation.
fn re10_build_headers(
    ctx: &mut dyn NativeContext,
    entries: &[(String, String)],
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let hdrs0 = match ctx.new_object_initialized("com/sun/net/httpserver/Headers", "()V", &[])? {
        Some(Value::Object(Some(o))) => o,
        _ => return Err(ioex("Headers <init> failed")),
    };
    // GC-safety: `hdrs` is held across `create_string`/`add` allocations for
    // every entry. A moving GC mid-loop would otherwise leave the Rust-local
    // `hdrs` stale and silently drop every subsequent `add` (writes to a vacated
    // slot) — i.e. an incomplete request/response header map under GC pressure
    // (ES testHeaders / auth). Pin `hdrs` for the whole build, and pin each key
    // string across the value-string allocation that follows it; re-read the
    // forwarded addresses before the `add`. The just-created value string `vs`
    // is used immediately (no allocation before the invoke) so it needs no pin.
    let h_pin = ctx.pin_native_root(hdrs0);
    for (k, v) in entries {
        let ks0 = ctx.create_string(k);
        let ks_pin = ctx.pin_native_root(ks0);
        let vs = ctx.create_string(v);
        let hdrs = ctx.read_native_pin(h_pin, hdrs0);
        let ks = ctx.read_native_pin(ks_pin, ks0);
        let _ = ctx.invoke(
            "com/sun/net/httpserver/Headers",
            "add",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            &[
                Value::Object(Some(hdrs)),
                Value::Object(Some(ks)),
                Value::Object(Some(vs)),
            ],
        );
        // Pop just this iteration's key-string pin (keep `hdrs` pinned).
        ctx.unpin_native_roots(ks_pin);
    }
    let hdrs = ctx.read_native_pin(h_pin, hdrs0);
    ctx.unpin_native_roots(h_pin);
    Ok(hdrs)
}

/// Read a real `Map<String,List<String>>` (the response Headers the handler
/// populated) into flat (name, value) pairs via its polymorphic
/// `entrySet().iterator()` — the same layout-agnostic walk the collections
/// crate uses for unmodelled maps.
fn re10_read_headers(ctx: &mut dyn NativeContext, map: ObjectRef) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let set = match ctx.invoke(
        "java/util/Map",
        "entrySet",
        "()Ljava/util/Set;",
        &[Value::Object(Some(map))],
    ) {
        Ok(Some(Value::Object(Some(s)))) => s,
        _ => return out,
    };
    let it0 = match ctx.invoke(
        "java/util/Set",
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(set))],
    ) {
        Ok(Some(Value::Object(Some(i)))) => i,
        _ => return out,
    };
    // GC-safety: the iterator (`it`), each `entry`, and each value `List` are
    // held across the per-element `hasNext`/`next`/`getKey`/`getValue`/`size`/
    // `get` invokes, every one of which allocates. A moving GC mid-walk would
    // leave a Rust-local stale and truncate/garble the read header map (ES
    // testHeaders). Pin the iterator for the whole walk and each entry / value
    // list across the invokes that consume it; the key/value Strings are turned
    // into Rust `String`s immediately (no held ObjectRef). `it_pin` is the base;
    // per-iteration pins are popped before the next round so the pin stack stays
    // bounded.
    let it_pin = ctx.pin_native_root(it0);
    loop {
        let it = ctx.read_native_pin(it_pin, it0);
        let has = matches!(
            ctx.invoke(
                "java/util/Iterator",
                "hasNext",
                "()Z",
                &[Value::Object(Some(it))]
            ),
            Ok(Some(Value::Int(1)))
        );
        if !has {
            break;
        }
        let it = ctx.read_native_pin(it_pin, it0);
        let entry0 = match ctx.invoke(
            "java/util/Iterator",
            "next",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(it))],
        ) {
            Ok(Some(Value::Object(Some(e)))) => e,
            _ => break,
        };
        let entry_pin = ctx.pin_native_root(entry0);
        let key = match ctx.invoke(
            "java/util/Map$Entry",
            "getKey",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(entry0))],
        ) {
            Ok(Some(Value::Object(Some(k)))) => ctx.read_string(k).unwrap_or_default(),
            _ => {
                ctx.unpin_native_roots(entry_pin);
                continue;
            }
        };
        let entry = ctx.read_native_pin(entry_pin, entry0);
        let val_list0 = match ctx.invoke(
            "java/util/Map$Entry",
            "getValue",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(entry))],
        ) {
            Ok(Some(Value::Object(Some(l)))) => l,
            _ => {
                ctx.unpin_native_roots(entry_pin);
                continue;
            }
        };
        // `entry` is done; reuse its pin slot for the value list.
        ctx.unpin_native_roots(entry_pin);
        let vl_pin = ctx.pin_native_root(val_list0);
        let n = match ctx.invoke(
            "java/util/List",
            "size",
            "()I",
            &[Value::Object(Some(val_list0))],
        ) {
            Ok(Some(Value::Int(n))) => n,
            _ => 0,
        };
        for i in 0..n {
            let val_list = ctx.read_native_pin(vl_pin, val_list0);
            if let Ok(Some(Value::Object(Some(s)))) = ctx.invoke(
                "java/util/List",
                "get",
                "(I)Ljava/lang/Object;",
                &[Value::Object(Some(val_list)), Value::Int(i)],
            ) {
                out.push((key.clone(), ctx.read_string(s).unwrap_or_default()));
            }
        }
        ctx.unpin_native_roots(vl_pin);
    }
    ctx.unpin_native_roots(it_pin);
    out
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
        let (status, body_bytes, resp_headers, len_hint) = match handler_info {
            Some(h) => {
                // GC-safety: this native dispatcher holds the handler (`h`) and
                // the HttpExchange (`ex`) across many VM allocations below
                // (create_string, build-headers, byte arrays, ref array, and the
                // handler invoke). A bare ObjectRef in a Rust local does NOT
                // survive a moving GC those allocations may trigger — it goes
                // stale, which surfaced as a `NoSuchMethodError
                // java/lang/Object.handle` storm (stale receiver) and corrupted
                // exchange field writes (landing in a vacated from-space slot)
                // under -Xmx1g GC pressure (ES testManyAsyncRequests /
                // testHeaders). Pin both as native roots and re-read the
                // forwarded address (`read_native_pin`) after every allocation.
                // Each freshly-created field VALUE is set immediately (no
                // allocation between its create and its set), so values need no
                // pin. `unpin_native_roots(h_pin)` releases the whole batch.
                let h_pin = ctx.pin_native_root(h);
                let ex0 = alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/HttpExchange", 8);
                let ex_pin = ctx.pin_native_root(ex0);

                let m = ctx.create_string(&req.method);
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, 0, Value::Object(Some(m)));
                let u = ctx.create_string(&req.uri);
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, 1, Value::Object(Some(u)));
                // Request + response headers are REAL `Headers` (HashMap subclass)
                // objects so the handler's `entrySet()`/`put()`/`getFirst()` run
                // real bytecode. The synthetic Headers had no Map methods.
                let rh = match re10_build_headers(ctx, &req.headers) {
                    Ok(r) => r,
                    Err(e) => {
                        ctx.unpin_native_roots(h_pin);
                        return Err(e);
                    }
                };
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, 2, Value::Object(Some(rh)));
                let rsph = match re10_build_headers(ctx, &[]) {
                    Ok(r) => r,
                    Err(e) => {
                        ctx.unpin_native_roots(h_pin);
                        return Err(e);
                    }
                };
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, 3, Value::Object(Some(rsph)));
                let body_arr = new_java_byte_array(ctx, &req.body);
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, 4, Value::Object(Some(body_arr)));
                ctx.set_field(ex, 5, Value::Int(200));
                let resp_body_chunks = ctx.new_ref_array(ClassId::new(0), 64);
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, 6, Value::Object(Some(resp_body_chunks)));
                ctx.set_field(ex, 7, Value::Int(0));

                // Re-read both pinned roots immediately before the invoke (the
                // exchange-build allocations above may have relocated them).
                let h = ctx.read_native_pin(h_pin, h);
                let ex = ctx.read_native_pin(ex_pin, ex0);
                let _ = ctx.invoke_virtual(
                    h,
                    "handle",
                    "(Lcom/sun/net/httpserver/HttpExchange;)V",
                    &[Value::Object(Some(ex))],
                );
                // The handler ran bytecode (allocations) — refresh `ex` before
                // reading its populated result fields.
                let ex = ctx.read_native_pin(ex_pin, ex0);
                let status = ctx.get_field(ex, 5).as_int().unwrap_or(200);
                let mut body_bytes: Vec<u8> = Vec::new();
                if let Value::Object(Some(chunks)) = ctx.get_field(ex, 6) {
                    // array_length / get_array_element do not allocate, so the
                    // chunk array and each `ba` stay valid through the walk.
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
                let ex = ctx.read_native_pin(ex_pin, ex0);
                let resp_headers = match ctx.get_field(ex, 3) {
                    Value::Object(Some(rh)) => re10_read_headers(ctx, rh),
                    _ => Vec::new(),
                };
                let ex = ctx.read_native_pin(ex_pin, ex0);
                // Response-length hint from sendResponseHeaders: -1 => no body /
                // no Content-Length (HEAD, 204, 304).
                let len_hint = ctx.get_field(ex, 7).as_int().unwrap_or(0);
                ctx.unpin_native_roots(h_pin);
                (status, body_bytes, resp_headers, len_hint)
            }
            None => (404, b"Not Found".to_vec(), Vec::new(), 0),
        };
        let stream = req.stream;
        let mut resp = Vec::with_capacity(128 + body_bytes.len());
        use std::io::Write as _;
        let _ = write!(&mut resp, "HTTP/1.1 {status} {}\r\n", http_reason(status));
        for (k, v) in &resp_headers {
            // The server owns the message-framing headers (Content-Length,
            // Connection, Transfer-Encoding) and the Date header; the real
            // com.sun.net.httpserver emits exactly one of each and ignores any
            // handler-echoed copy. A handler that copies request headers into the
            // response (as the ES ResponseHandler does) would otherwise produce a
            // DUPLICATE Connection/Content-Length, which the client surfaces as an
            // extra header (ES testHeaders). Skip those here and emit our own.
            if k.eq_ignore_ascii_case("content-length")
                || k.eq_ignore_ascii_case("connection")
                || k.eq_ignore_ascii_case("transfer-encoding")
                || k.eq_ignore_ascii_case("date")
            {
                continue;
            }
            let _ = write!(&mut resp, "{k}: {v}\r\n");
        }
        // Server-controlled framing headers, each exactly once (matches the real
        // com.sun.net.httpserver, which auto-adds Date + Content-length).
        let _ = write!(&mut resp, "Date: {}\r\n", http_date_now());
        // A HEAD response carries no body and (per the real server / ES
        // testHeaders) no Content-Length. Every other method gets a
        // Content-Length — including 0 for an empty body — and the body bytes.
        // (`len_hint` from sendResponseHeaders is captured but the HEAD method is
        // the distinction the client/test actually keys on.)
        let _ = len_hint;
        let is_head = req.method.eq_ignore_ascii_case("HEAD");
        if !is_head {
            // Casing matters: the real com.sun.net.httpserver emits "Content-length"
            // (lowercase 'l') and ES assertHeaders compares header names
            // case-sensitively against that exact spelling.
            let _ = write!(&mut resp, "Content-length: {}\r\n", body_bytes.len());
        }
        resp.extend_from_slice(b"Connection: close\r\n\r\n");
        if !is_head {
            resp.extend_from_slice(&body_bytes);
        }
        // Write the response and close on a short-lived I/O thread (pure socket
        // work, no VM context needed) so the dispatcher returns immediately to
        // serve the next queued request instead of blocking on the per-connection
        // lingering close.
        re10_send_response(stream, resp);
    }
    Ok(drained)
}

/// Write a fully-formed HTTP response to `stream` and close it gracefully on a
/// detached thread. The close is a *lingering* close: send FIN (`shutdown(Write)`)
/// then drain the read side until the peer closes (EOF) before dropping the
/// socket. This avoids the Windows RST-on-close that resets a peer still reading
/// the response (a non-blocking Apache NIO reactor surfaced this as
/// "connection reset" / os error 10053); a bare `shutdown(Both)` discards unread
/// bytes and is RST-prone, while no drain at all races the OS close against the
/// client's read.
fn re10_send_response(mut stream: TcpStream, resp: Vec<u8>) {
    let _ = std::thread::Builder::new()
        .name("cratonvm-httpserver-resp".to_string())
        .spawn(move || {
            use std::io::{Read as _, Write as _};
            let _ = stream.write_all(&resp);
            let _ = stream.flush();
            let _ = stream.shutdown(std::net::Shutdown::Write);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut drain = [0u8; 512];
            loop {
                match stream.read(&mut drain) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        });
}

/// Synthetic Runnable whose `run()` drives the per-server HTTP dispatch loop on
/// a real VM thread. Field 0 holds the `server_id`.
const HS_LOOP_CLASS: &str = "CratonVM$HttpServerLoop";

// Requested field count for the worker `java/lang/Thread`. The VM may hand back
// the real-JDK layout (more fields) instead — `Thread.<init>` handles both.
const HS_THREAD_NUM_FIELDS: usize = 5;

/// Build a daemon VM thread whose `run()` is `re10_serve_loop_run` for
/// `server_id`, and start it via the VM's real thread machinery. The thread
/// exits when the server's `running` flag clears (stop()).
/// Number of VM dispatcher threads draining the request queue concurrently.
/// More than one lets independent requests run their Java handlers in parallel
/// (each request is popped by exactly one thread), which is what keeps a burst
/// of hundreds of concurrent requests (ES testManyAsyncRequests) inside the
/// client's timeout instead of serializing every handler on one thread.
const HS_DISPATCHER_POOL: i32 = 4;

fn re10_spawn_dispatcher(
    ctx: &mut dyn NativeContext,
    server_id: i32,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let dbg = std::env::var_os("CRATONVM_DBG_HTTPSRV").is_some();
    for idx in 0..HS_DISPATCHER_POOL {
        let runner = alloc_concurrent_synthetic(ctx, HS_LOOP_CLASS, 1);
        ctx.set_field(runner, 0, Value::Int(server_id));

        let worker = alloc_concurrent_synthetic(ctx, "java/lang/Thread", HS_THREAD_NUM_FIELDS);
        let name = ctx.create_string(&format!("cratonvm-httpserver-dispatch-{server_id}-{idx}"));
        // Populate the worker Thread via the registered
        // `Thread.<init>(ThreadGroup, Runnable, String)` native. This stores the
        // runnable the right way for BOTH layouts: slot 3 (`target`) on a
        // synthetic <=8-field Thread, or `holder:FieldHolder.task` on a real-JDK
        // Thread — which is what `Thread.run()` actually reads. Setting `target`
        // by name on a real-JDK Thread does NOT work (no top-level `target`
        // field; the runnable lives in the FieldHolder), which is why the loop
        // never started before.
        let _ = ctx.invoke(
            "java/lang/Thread",
            "<init>",
            "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;)V",
            &[
                Value::Object(Some(worker)),
                Value::Object(None),
                Value::Object(Some(runner)),
                Value::Object(Some(name)),
            ],
        );
        // Daemon so a server left unstopped never wedges VM shutdown after main()
        // returns (it normally exits on stop() when `running` clears).
        let _ = ctx.invoke(
            "java/lang/Thread",
            "setDaemon",
            "(Z)V",
            &[Value::Object(Some(worker)), Value::Int(1)],
        );
        // Best-effort: if the VM has no thread registry (e.g. test mocks) the
        // start is a no-op; start()/stop() still drain the queue as a fallback.
        let res = ctx.thread_start(worker);
        if dbg {
            eprintln!(
                "[HTTPSRV] spawn_dispatcher server={server_id} idx={idx} num_fields={} thread_start_ok={}",
                ctx.object_num_fields(worker),
                res.is_ok()
            );
        }
    }
    Ok(())
}

/// `CratonVM$HttpServerLoop.run()` — runs on a dedicated VM thread. Drains the
/// inbound request queue and dispatches each request through the real Java
/// `HttpHandler`, then idles (in a GC-blocked region) until more arrive. Exits
/// when the server's `running` flag clears.
fn re10_serve_loop_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let server_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
    let dbg = std::env::var_os("CRATONVM_DBG_HTTPSRV").is_some();
    if dbg {
        eprintln!("[HTTPSRV] serve_loop ENTER server={server_id}");
    }
    if server_id < 0 {
        return Ok(None);
    }
    loop {
        let running = server_registry()
            .lock()
            .get(&server_id)
            .map(|s| s.running.load(Ordering::SeqCst))
            .unwrap_or(false);
        if !running {
            if dbg {
                eprintln!("[HTTPSRV] serve_loop EXIT server={server_id} (not running)");
            }
            break;
        }
        // Dispatch runs Java bytecode (the handler) which cooperates with
        // safepoints normally — only the idle wait needs a blocking region.
        let drained = re10_dispatch_pending(ctx, server_id)?;
        if drained == 0 {
            ctx.begin_blocking_region();
            std::thread::sleep(Duration::from_millis(2));
            ctx.end_blocking_region();
        }
    }
    Ok(None)
}

/// Current time as an RFC 1123 HTTP-date (e.g. "Thu, 18 Jun 2026 08:37:05 GMT").
/// The real `com.sun.net.httpserver` adds a `Date` response header automatically;
/// clients (and the ES `testHeaders` assertion) expect it.
fn http_date_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_http_date(secs)
}

/// Format epoch seconds as an RFC 1123 date in GMT (civil-from-days per
/// Howard Hinnant's algorithm; no external date crate).
fn format_http_date(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86400) as i64;
    let rem = epoch_secs % 86400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 1970-01-01 is a Thursday; Sun=0.
    let dow = (((days % 7) + 4) % 7) as usize;
    let dow_name = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][dow];
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    let mon_name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(m - 1) as usize];
    format!("{dow_name}, {d:02} {mon_name} {year} {hh:02}:{mm:02}:{ss:02} GMT")
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
        // VULN-FIX [nb-net-phase-e]: 413 used to reject oversized request bodies
        // (Content-Length exceeding the configurable max — see http_max_request_body()).
        413 => "Payload Too Large",
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
    if state.listener.lock().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "not bound",
        ));
    }
    let state_cl = state.clone();
    std::thread::Builder::new()
        .name(format!("cratonvm-httpserver-{server_id}"))
        .spawn(move || {
            while state_cl.running.load(Ordering::SeqCst) {
                // Accept under the listener lock (the listener is non-blocking, so
                // this returns immediately). stop() takes the listener out from
                // under us to close it synchronously, after which `as_ref()` is
                // None and we exit.
                let accepted = {
                    let guard = state_cl.listener.lock();
                    match guard.as_ref() {
                        Some(l) => l.accept(),
                        None => break,
                    }
                };
                match accepted {
                    Ok((stream, _peer)) => {
                        if !state_cl.running.load(Ordering::SeqCst) {
                            drop(stream);
                            break;
                        }
                        // Parse each connection on its own short-lived thread so a
                        // slow (or merely not-yet-written) request never blocks the
                        // accept loop. Serially parsing here let the OS listen
                        // backlog overflow under burst load (hundreds of concurrent
                        // Connection: close requests), failing requests — see ES
                        // testManyAsyncRequests. Parse is pure socket work and needs
                        // no VM context; the dispatcher thread(s) run the handler.
                        let sid = server_id;
                        let _ = std::thread::Builder::new()
                            .name(format!("cratonvm-httpserver-parse-{sid}"))
                            .spawn(move || {
                                if let Some(pending) = parse_http_request(stream) {
                                    let mut q = request_queue().lock();
                                    q.entry(sid).or_default().push(pending);
                                }
                            });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        })?;
    Ok(())
}

fn register_re10_http_server(r: &mut NativeMethodRegistry) {
    let hs = "com/sun/net/httpserver/HttpServer";
    // The JDK factory contract returns this concrete implementation, not the
    // abstract public API class. Native bridges are aliased to it after all
    // registrations below so virtual dispatch retains the concrete receiver.

    // VM-thread dispatch loop runner (see re10_spawn_dispatcher).
    r.register(HS_LOOP_CLASS, "run", "()V", re10_serve_loop_run);

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
            // Non-blocking so the accept loop polls `running` (and so it can be
            // closed promptly by stop()).
            listener.set_nonblocking(true).ok();
            let bound_addr = listener.local_addr().ok();
            let bound_port = bound_addr.map(|a| a.port() as i32).unwrap_or(port);
            let bound_ip = bound_addr.map(|a| a.ip()).unwrap_or(ip);
            let server_id = next_server_id();
            let state = std::sync::Arc::new(ServerState {
                listener: Mutex::new(Some(listener)),
                running: AtomicBool::new(false),
                handlers: Mutex::new(Vec::new()),
                bound_port: AtomicI32::new(bound_port),
            });
            server_registry().lock().insert(server_id, state);
            // The public class is abstract, but this native-backed server owns
            // the concrete implementation. Keep state for every abstract
            // method we bridge below, including the optional Executor.
            let srv = alloc_concurrent_synthetic(ctx, HS_IMPL_CLASS, 6);
            // Fully-resolved echo (matches HotSpot: `getAddress()` returns
            // the socket's ACTUAL bound address, not the caller's original
            // hostname string) — see `alloc_inet_socket_address_resolved`.
            let bound_ip_str = bound_ip.to_string();
            let sa_echo =
                alloc_inet_socket_address_resolved(ctx, &bound_ip_str, &bound_ip_str, bound_port);
            ctx.set_field(srv, HS_ADDRESS, Value::Object(Some(sa_echo)));
            ctx.set_field(srv, HS_STARTED, Value::Int(0));
            ctx.set_field(srv, HS_CONTEXTS, Value::Object(None));
            ctx.set_field(srv, HS_SERVER_ID, Value::Int(server_id));
            ctx.set_field(srv, HS_PORT, Value::Int(bound_port));
            ctx.set_field(srv, HS_EXECUTOR, Value::Object(None));
            let _ = backlog;
            Ok(Some(Value::Object(Some(srv))))
        },
    );

    // `HttpServer` declares these methods abstract. `create` returns the
    // native-backed receiver above, so both calls must be registered here;
    // otherwise virtual dispatch resolves the abstract declaration and throws
    // `AbstractMethodError: ... has no Code attribute` before a server can
    // start (Keycloak's shared startHttpServer helper exercises this path).
    r.register(
        hs,
        "setExecutor",
        "(Ljava/util/concurrent/Executor;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if ctx.get_field(this, HS_STARTED).as_int().unwrap_or(0) != 0 {
                return Err(RuntimeError::IllegalStateException {
                    message: "server already started".to_string(),
                }
                .into());
            }
            ctx.set_field(
                this,
                HS_EXECUTOR,
                args.get(1).copied().unwrap_or(Value::Object(None)),
            );
            Ok(None)
        },
    );
    r.register(
        hs,
        "getExecutor",
        "()Ljava/util/concurrent/Executor;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, HS_EXECUTOR)))
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
        // The OS accept thread (re10_start_server) only parses requests into the
        // queue — it has no VM context and cannot invoke the Java `HttpHandler`.
        // Spawn a dedicated VM thread whose `run()` drains that queue and
        // dispatches each request on a thread that CAN run Java bytecode. Without
        // this, requests were only dispatched on `start()`/`stop()`, so a client
        // that blocks waiting for a response (e.g. the ES `RestClient*IntegTests`)
        // deadlocks against the embedded server. See ES-HANG-02.
        re10_spawn_dispatcher(ctx, id)?;
        Ok(None)
    });

    r.register(hs, "stop", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
        if id >= 0 {
            if let Some(state) = server_registry().lock().get(&id) {
                state.running.store(false, Ordering::SeqCst);
                // Close the OS listener NOW (drop it) so the port immediately
                // refuses connections — a round-robin client must see a stopped
                // host fail fast and retry, not connect into a dead server in the
                // window before the accept thread notices `running`.
                state.listener.lock().take();
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

    // removeContext(String path) — drop the handler registered for that exact
    // path (ES MultipleHosts resetWaitHandlers swaps the "/wait" handler).
    r.register(hs, "removeContext", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = value_or_string(ctx, args.get(1).copied().unwrap_or(Value::Object(None)), "");
        let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
        if id >= 0 {
            if let Some(state) = server_registry().lock().get(&id) {
                state.handlers.lock().retain(|e| e.path_prefix != path);
            }
        }
        Ok(None)
    });
    // removeContext(HttpContext ctx) — same, resolving the path from the context's
    // slot 0 (set by createContext).
    r.register(
        hs,
        "removeContext",
        "(Lcom/sun/net/httpserver/HttpContext;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
            if let Some(Value::Object(Some(hctx))) = args.get(1).copied() {
                let path = value_or_string(ctx, ctx.get_field(hctx, 0), "");
                if id >= 0 {
                    if let Some(state) = server_registry().lock().get(&id) {
                        state.handlers.lock().retain(|e| e.path_prefix != path);
                    }
                }
            }
            Ok(None)
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
    r.register(
        hex,
        "getRequestMethod",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(hex, "getRequestURI", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // The exchange stores the request target as a String. Do not place it
        // in a guessed URI field slot: in the real JDK layout slot 0 is the
        // scheme, not the full external form, which made toString()/getPath()
        // observe an empty URI. `make_uri` writes the canonical URI fields by
        // name and therefore works for both synthetic and real-JDK layouts.
        let raw = match ctx.get_field(this, 1) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let uri = make_uri(ctx, &raw);
        Ok(Some(Value::Object(Some(uri))))
    });
    // Request/response headers are stored on the exchange as REAL
    // `com.sun.net.httpserver.Headers` (see re10_dispatch_pending) — return them
    // directly so the handler operates on a live Map.
    r.register(
        hex,
        "getResponseHeaders",
        "()Lcom/sun/net/httpserver/Headers;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        },
    );
    r.register(
        hex,
        "getRequestHeaders",
        "()Lcom/sun/net/httpserver/Headers;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        },
    );
    r.register(
        hex,
        "getRequestBody",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let body0 = match ctx.get_field(this, 4) {
                Value::Object(Some(a)) => a,
                _ => ctx.new_array(ArrayElementType::Byte, 0),
            };
            let len = ctx.array_length(body0) as i32;
            // GC-safety: `body0` is read here but used only AFTER the
            // `ByteArrayInputStream` allocation below, which can trigger a moving
            // GC. A stale `body` would set `ByteArrayInputStream.buf` to a vacated
            // from-space slot, so the handler reads garbage request bytes and
            // typically throws while decoding — aborting `handle()` BEFORE it
            // calls `sendResponseHeaders(status)`, which the dispatcher then
            // reports as the default 200 (observed as ES auth-test
            // `expected:<403> but was:<200>` under -Xmx1g GC pressure). Pin it
            // across the alloc and read the forwarded address back.
            let body_pin = ctx.pin_native_root(body0);
            let stream = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
            let body = ctx.read_native_pin(body_pin, body0);
            ctx.set_field(stream, 0, Value::Object(Some(body))); // buf
            ctx.set_field(stream, 1, Value::Int(0)); // pos
            ctx.set_field(stream, 2, Value::Int(0)); // mark
            ctx.set_field(stream, 3, Value::Int(len)); // count
            ctx.unpin_native_roots(body_pin);
            Ok(Some(Value::Object(Some(stream))))
        },
    );
    r.register(hex, "sendResponseHeaders", "(IJ)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let code = args.get(1).and_then(|v| v.as_int()).unwrap_or(200);
        ctx.set_field(this, 5, Value::Int(code));
        // Stash the response-length argument (field 7): per the
        // com.sun.net.httpserver contract, -1 means "no response body and NO
        // Content-Length header" (HEAD / 204 / 304); >0 is the body length; 0
        // means chunked. We honour -1 so HEAD responses don't carry a spurious
        // Content-Length (ES testHeaders). The Long arrives at args[2].
        let len = match args.get(2) {
            Some(Value::Long(l)) => *l,
            Some(v) => v.as_int().map(|i| i as i64).unwrap_or(0),
            None => 0,
        };
        ctx.set_field(
            this,
            7,
            Value::Int(len.clamp(i32::MIN as i64, i32::MAX as i64) as i32),
        );
        Ok(None)
    });
    r.register(
        hex,
        "getResponseBody",
        "()Ljava/io/OutputStream;",
        |ctx, args| {
            // GC-safety: `this` (the exchange) is used AFTER the ResponseBody
            // allocation, which can move it. A stale `this` would set
            // `ResponseBody.owner` (slot 0) to a vacated exchange address, so a
            // later `out.write(...)` reads the wrong/garbage owner and drops the
            // response body. Pin it across the alloc and read it back forwarded.
            let this0 = obj_arg(args, 0)?;
            let this_pin = ctx.pin_native_root(this0);
            let out = alloc_concurrent_synthetic(
                ctx,
                "com/sun/net/httpserver/HttpExchange$ResponseBody",
                2,
            );
            let this = ctx.read_native_pin(this_pin, this0);
            ctx.set_field(out, 0, Value::Object(Some(this)));
            ctx.set_field(out, 1, Value::Int(0));
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(out))))
        },
    );
    r.register(hex, "close", "()V", |_ctx, _args| Ok(None));

    let rb = "com/sun/net/httpserver/HttpExchange$ResponseBody";
    r.register(rb, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner0 = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Err(ioex("ResponseBody has no exchange")),
        };
        let buf = obj_arg(args, 1)?;
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let data = java_byte_array_to_vec(ctx, buf, off, len)?;
        // GC-safety: `owner` (the exchange) is used after `new_java_byte_array`,
        // which can relocate it. Pin across the alloc and read it back forwarded
        // so the chunk lands in the live exchange's chunk array, not a vacated
        // from-space slot (which would silently drop the response body).
        let owner_pin = ctx.pin_native_root(owner0);
        let chunk = new_java_byte_array(ctx, &data);
        let owner = ctx.read_native_pin(owner_pin, owner0);
        if let Value::Object(Some(chunks)) = ctx.get_field(owner, 6) {
            let cap = ctx.array_length(chunks);
            for i in 0..cap {
                if let Value::Object(None) = ctx.get_array_element(chunks, i) {
                    ctx.set_array_element(chunks, i, Value::Object(Some(chunk)));
                    break;
                }
            }
        }
        ctx.unpin_native_roots(owner_pin);
        Ok(None)
    });
    // `OutputStream.write(byte[])` — the synthetic ResponseBody does not inherit
    // the real OutputStream default (no superclass bytecode), so register the
    // overload explicitly. Delegates to the same append-a-chunk logic as
    // write([BII) over the whole array.
    r.register(rb, "write", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner0 = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Err(ioex("ResponseBody has no exchange")),
        };
        let buf = obj_arg(args, 1)?;
        let len = ctx.array_length(buf) as i32;
        let data = java_byte_array_to_vec(ctx, buf, 0, len)?;
        // GC-safety: see write([BII) — pin `owner` across the chunk allocation.
        let owner_pin = ctx.pin_native_root(owner0);
        let chunk = new_java_byte_array(ctx, &data);
        let owner = ctx.read_native_pin(owner_pin, owner0);
        if let Value::Object(Some(chunks)) = ctx.get_field(owner, 6) {
            let cap = ctx.array_length(chunks);
            for i in 0..cap {
                if let Value::Object(None) = ctx.get_array_element(chunks, i) {
                    ctx.set_array_element(chunks, i, Value::Object(Some(chunk)));
                    break;
                }
            }
        }
        ctx.unpin_native_roots(owner_pin);
        Ok(None)
    });
    r.register(rb, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner0 = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Err(ioex("ResponseBody has no exchange")),
        };
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) & 0xff;
        // GC-safety: see write([BII) — pin `owner` across the 1-byte chunk alloc.
        let owner_pin = ctx.pin_native_root(owner0);
        let chunk = ctx.new_array(ArrayElementType::Byte, 1);
        ctx.set_array_element(chunk, 0, Value::Int(b as i8 as i32));
        let owner = ctx.read_native_pin(owner_pin, owner0);
        if let Value::Object(Some(chunks)) = ctx.get_field(owner, 6) {
            let cap = ctx.array_length(chunks);
            for i in 0..cap {
                if let Value::Object(None) = ctx.get_array_element(chunks, i) {
                    ctx.set_array_element(chunks, i, Value::Object(Some(chunk)));
                    break;
                }
            }
        }
        ctx.unpin_native_roots(owner_pin);
        Ok(None)
    });
    r.register(rb, "flush", "()V", |_ctx, _args| Ok(None));
    r.register(rb, "close", "()V", |_ctx, _args| Ok(None));

    // Native dispatch is keyed by the receiver class rather than Java
    // inheritance. Mirror the complete public HttpServer bridge surface onto
    // the concrete class returned by the factory, including set/getExecutor.
    r.alias_class(hs, HS_IMPL_CLASS);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::NativeContext;

    #[test]
    fn re1_http_parse_url_plain() {
        let (https, host, port, path, userinfo) = http_parse_url("http://example.com/foo").unwrap();
        assert!(!https);
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
        assert_eq!(path, "/foo");
        assert_eq!(userinfo, None);
    }

    #[test]
    fn re1_http_parse_url_with_port() {
        let (https, host, port, path, userinfo) =
            http_parse_url("https://example.com:8443/api?x=1").unwrap();
        assert!(https);
        assert_eq!(host, "example.com");
        assert_eq!(port, 8443);
        assert_eq!(path, "/api?x=1");
        assert_eq!(userinfo, None);
    }

    #[test]
    fn re1_http_parse_url_with_userinfo() {
        // user-info must be stripped from the connect target / Host header and
        // returned separately (ResourceTests.useUserInfoToSetBasicAuth).
        let (https, host, port, path, userinfo) =
            http_parse_url("http://alice:secret@localhost:8080/resource").unwrap();
        assert!(!https);
        assert_eq!(host, "localhost");
        assert_eq!(port, 8080);
        assert_eq!(path, "/resource");
        assert_eq!(userinfo.as_deref(), Some("alice:secret"));
    }

    #[test]
    fn re1_field5_full_url_discriminator() {
        // Full URLs (scheme-carrying) are accepted…
        assert!(field5_is_full_url("http://localhost:8080/x"));
        assert!(field5_is_full_url("file:/tmp/x.txt"));
        assert!(field5_is_full_url("jar:file:/a.jar!/e"));
        // …while authorities are rejected: bare host:port (digits after ':')
        // and user-info-carrying authorities ('@' before any '/').
        assert!(!field5_is_full_url("localhost:8080"));
        assert!(!field5_is_full_url("alice:secret@localhost:8080"));
        assert!(!field5_is_full_url("alice@localhost:8080"));
    }

    #[test]
    fn uri_split_keeps_opaque_question_mark_inside_ssp() {
        let (scheme, authority, path, query, fragment) =
            uri_split("mailto:user@example.com?subject=hello#frag");

        assert_eq!(scheme.as_deref(), Some("mailto"));
        assert_eq!(authority, None);
        assert_eq!(path, "user@example.com?subject=hello");
        assert_eq!(query, None);
        assert_eq!(fragment.as_deref(), Some("frag"));
    }

    #[test]
    fn uri_scheme_specific_part_excludes_fragment() {
        assert_eq!(
            uri_raw_scheme_specific_part("mailto:foo@bar.com#baz"),
            "foo@bar.com"
        );
        assert_eq!(
            uri_raw_scheme_specific_part("mailto:user@example.com?subject=hello"),
            "user@example.com?subject=hello"
        );
        assert_eq!(
            uri_raw_scheme_specific_part("https://example.com/foo?bar#baz"),
            "//example.com/foo?bar"
        );
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
    fn re5_http_request_timeout_bounds_delayed_response_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        });

        let error = match http_perform_request_with_timeout(
            "GET",
            &format!("http://127.0.0.1:{port}/slow"),
            &[],
            &[],
            Duration::from_millis(10),
            0,
        ) {
            Ok(_) => panic!("a delayed response head must exceed the request deadline"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("timed out"));
        server.join().unwrap();
    }

    #[test]
    fn re1_socket_side_table_is_identity_keyed() {
        let mut ctx = MockNativeContext::new();
        let sock = match ctx.new_object("java/net/Socket").unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected socket object, got {other:?}"),
        };

        sock_set(&ctx, sock, |s| {
            s.port = 9999;
            s.stream_id = 123;
        });

        let side = sock_get(&ctx, sock);
        assert_eq!(side.port, 9999);
        assert_eq!(side.stream_id, 123);
        assert_eq!(
            native_obj_key(&ctx, sock).identity,
            ctx.identity_hash_code(sock)
        );
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

    // Regression (BUG-TC0622 Gap A): an IPv4-mapped IPv6 literal must fold to
    // its IPv4 dotted-quad so the `InetAddress` mirror is an `Inet4Address`
    // (4-byte getAddress) — otherwise Tomcat's RemoteIpFilter can't match a
    // dual-stack loopback peer against `127.0.0.0/8`. Genuine IPv6 must render in
    // HotSpot's full uncompressed eight-group form (NOT Rust's RFC-5952 `::`),
    // and plain IPv4 / non-IP hosts pass through byte-identical.
    #[test]
    fn hotspot_ip_string_matches_jdk_text_form() {
        // v4-mapped fold → Inet4Address dotted-quad.
        assert_eq!(hotspot_ip_string("::ffff:127.0.0.1"), "127.0.0.1");
        assert_eq!(hotspot_ip_string("::ffff:10.1.2.3"), "10.1.2.3");
        // Genuine IPv6: HotSpot's Inet6Address.numericToTextFormat — eight
        // minimal-hex groups joined by ':', no zero-compression.
        assert_eq!(hotspot_ip_string("::1"), "0:0:0:0:0:0:0:1");
        assert_eq!(hotspot_ip_string("fe80::1"), "fe80:0:0:0:0:0:0:1");
        assert_eq!(hotspot_ip_string("2001:db8::1"), "2001:db8:0:0:0:0:0:1");
        // Plain IPv4 and non-IP hosts: untouched.
        assert_eq!(hotspot_ip_string("127.0.0.1"), "127.0.0.1");
        assert_eq!(hotspot_ip_string("example.com"), "example.com");
    }

    #[test]
    fn re4_http_decode_chunked() {
        let raw = b"5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n";
        let body = http_decode_chunked(raw).unwrap();
        assert_eq!(body, b"helloworld");
    }

    // FIX(net-phase-e #1): a crafted chunk size must never panic — neither on
    // numeric overflow of the hex parse nor on the `n + 2` / `&data[..n]`
    // arithmetic. All malformed/oversize sizes must return a protocol error.
    #[test]
    fn re4_http_decode_chunked_overflow_is_error_not_panic() {
        // Hex value that overflows usize (would have panicked `n + 2` / slice).
        let huge = b"ffffffffffffffff\r\nx\r\n0\r\n\r\n";
        assert!(http_decode_chunked(huge).is_err());

        // In-range-but-absurd size beyond MAX_CHUNK cap -> protocol error,
        // not an out-of-range slice panic.
        let big = b"7fffffff\r\nx\r\n0\r\n\r\n"; // ~2 GiB declared, tiny body
        assert!(http_decode_chunked(big).is_err());

        // Non-hex / empty chunk size -> protocol error.
        assert!(http_decode_chunked(b"zz\r\nx\r\n0\r\n\r\n").is_err());
        assert!(http_decode_chunked(b"\r\nx\r\n0\r\n\r\n").is_err());

        // A valid small chunk just over the available data is "truncated".
        assert!(http_decode_chunked(b"5\r\nhi\r\n").is_err());
    }

    #[test]
    fn re4_http_read_response_parses_status_and_body() {
        let raw = b"HTTP/1.1 204 No Content\r\nServer: t\r\nContent-Length: 0\r\n\r\n";
        let resp = http_read_response(&raw[..], false).unwrap();
        assert_eq!(resp.status, 204);
        assert_eq!(resp.body, Vec::<u8>::new());
        assert!(resp.headers.iter().any(|(k, _)| k == "Server"));
    }

    /// Serves exactly the given header block, then fails every further read
    /// with `WouldBlock` -- the shape of a keep-alive HEAD response: the
    /// headers legitimately declare the Content-Length the corresponding GET
    /// would have, but no body bytes ever arrive, and a blocking socket's
    /// `SO_RCVTIMEO` eventually surfaces EAGAIN ("Resource temporarily
    /// unavailable (os error 11)").
    struct HeadersThenEagain<'a>(&'a [u8]);

    impl Read for HeadersThenEagain<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "Resource temporarily unavailable (os error 11)",
                ));
            }
            let n = self.0.len().min(buf.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn re5_http_read_response_head_skips_declared_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\nContent-Encoding: gzip\r\n\r\n";
        // HEAD: must return immediately with an empty body, never touching
        // the socket again.
        let resp = http_read_response(HeadersThenEagain(&raw[..]), true).unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_empty());
        assert!(resp
            .headers
            .iter()
            .any(|(k, v)| k == "Content-Length" && v == "500"));
        // Non-HEAD control: the same wire bytes make the reader wait for the
        // declared 500 body bytes and hit the read timeout (the pre-fix
        // failure mode for HEAD).
        match http_read_response(HeadersThenEagain(&raw[..]), false) {
            Err(err) => assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock),
            Ok(_) => panic!("non-HEAD read must wait for the declared body"),
        }
    }

    #[test]
    fn re5_handler_tag_classifies_synthetic_vs_real_handlers() {
        let mut ctx = MockNativeContext::new();
        // Synthetic tagged handler -> its tag.
        let bh = alloc_concurrent_synthetic(&mut ctx, "java/net/http/HttpResponse$BodyHandler", 1);
        let tag = ctx.create_string("string");
        ctx.set_field(bh, 0, Value::Object(Some(tag)));
        assert_eq!(
            re5_handler_tag(&ctx, Some(Value::Object(Some(bh)))).as_deref(),
            Some("string")
        );
        // Null / absent handler degrades to the InputStream default.
        assert_eq!(re5_handler_tag(&ctx, None).as_deref(), Some("inputstream"));
        assert_eq!(
            re5_handler_tag(&ctx, Some(Value::Object(None))).as_deref(),
            Some("inputstream")
        );
        // A real user handler class -> None: drive the real protocol.
        let real = alloc_concurrent_synthetic(
            &mut ctx,
            "org/springframework/http/client/JdkClientHttpRequest$DecompressingBodyHandler",
            1,
        );
        assert_eq!(re5_handler_tag(&ctx, Some(Value::Object(Some(real)))), None);
    }

    /// Virtual-call hook that records `onNext`/`onComplete` deliveries by
    /// bumping counters in the receiver's own fields (slot 0 / slot 1).
    fn re5_recording_subscriber_hook(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        method_name: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        let slot = match method_name {
            "onNext" => 0,
            "onComplete" => 1,
            _ => return None,
        };
        let cur = ctx.get_field(receiver, slot).as_int().unwrap_or(0);
        ctx.set_field(receiver, slot, Value::Int(cur + 1));
        Some(Ok(None))
    }

    fn re5_test_replay_subscription(
        ctx: &mut MockNativeContext,
        subscriber: ObjectRef,
    ) -> ObjectRef {
        let subscription =
            alloc_concurrent_synthetic(ctx, RE5_REPLAY_SUBSCRIPTION, RE5_SUB_NUM_FIELDS);
        ctx.set_field(
            subscription,
            RE5_SUB_SUBSCRIBER,
            Value::Object(Some(subscriber)),
        );
        ctx.set_field(subscription, RE5_SUB_BODY, Value::Object(None));
        ctx.set_field(subscription, RE5_SUB_STATE, Value::Int(0));
        subscription
    }

    #[test]
    fn re5_replay_subscription_delivers_completion_exactly_once() {
        let mut ctx = MockNativeContext::new();
        ctx.set_invoke_virtual_hook(re5_recording_subscriber_hook);
        let subscriber = alloc_concurrent_synthetic(&mut ctx, "test/RecordingSubscriber", 2);
        let subscription = re5_test_replay_subscription(&mut ctx, subscriber);

        // Zero / negative demand: nothing delivered.
        for n in [0i64, -3] {
            re5_replay_subscription_request(
                &mut ctx,
                &[Value::Object(Some(subscription)), Value::Long(n)],
            )
            .unwrap();
        }
        assert_eq!(ctx.get_field(subscriber, 1), Value::Int(0));

        // First positive demand: exactly one onComplete (empty body -> no
        // onNext), and the parked references are dropped.
        re5_replay_subscription_request(
            &mut ctx,
            &[Value::Object(Some(subscription)), Value::Long(1)],
        )
        .unwrap();
        assert_eq!(ctx.get_field(subscriber, 0), Value::Int(0));
        assert_eq!(ctx.get_field(subscriber, 1), Value::Int(1));
        assert!(matches!(
            ctx.get_field(subscription, RE5_SUB_SUBSCRIBER),
            Value::Object(None)
        ));

        // One-shot: repeat demand delivers nothing further.
        re5_replay_subscription_request(
            &mut ctx,
            &[Value::Object(Some(subscription)), Value::Long(9)],
        )
        .unwrap();
        assert_eq!(ctx.get_field(subscriber, 1), Value::Int(1));
    }

    #[test]
    fn re5_replay_subscription_cancel_before_demand_suppresses_delivery() {
        let mut ctx = MockNativeContext::new();
        ctx.set_invoke_virtual_hook(re5_recording_subscriber_hook);
        let subscriber = alloc_concurrent_synthetic(&mut ctx, "test/RecordingSubscriber", 2);
        let subscription = re5_test_replay_subscription(&mut ctx, subscriber);

        re5_replay_subscription_cancel(&mut ctx, &[Value::Object(Some(subscription))]).unwrap();
        re5_replay_subscription_request(
            &mut ctx,
            &[Value::Object(Some(subscription)), Value::Long(1)],
        )
        .unwrap();
        assert_eq!(ctx.get_field(subscriber, 0), Value::Int(0));
        assert_eq!(ctx.get_field(subscriber, 1), Value::Int(0));
        assert!(matches!(
            ctx.get_field(subscription, RE5_SUB_SUBSCRIBER),
            Value::Object(None)
        ));
    }

    #[test]
    fn re5_body_publishers_of_byte_array_reads_static_arg_slot_zero() {
        let mut registry = NativeMethodRegistry::new();
        register_re5_http_client(&mut registry);
        let native = registry
            .find(
                "java/net/http/HttpRequest$BodyPublishers",
                "ofByteArray",
                "([B)Ljava/net/http/HttpRequest$BodyPublisher;",
            )
            .expect("ofByteArray native is registered");

        let mut ctx = MockNativeContext::new();
        let bytes = ctx.new_array(ArrayElementType::Byte, 3);
        ctx.set_array_element(bytes, 0, Value::Int(b'a' as i32));
        ctx.set_array_element(bytes, 1, Value::Int(b'b' as i32));
        ctx.set_array_element(bytes, 2, Value::Int(b'c' as i32));

        let publisher = match native(&mut ctx, &[Value::Object(Some(bytes))]).unwrap() {
            Some(Value::Object(Some(publisher))) => publisher,
            other => panic!("expected BodyPublisher object, got {other:?}"),
        };
        let body = match ctx.get_field(publisher, 0) {
            Value::Object(Some(body)) => body,
            other => panic!("expected publisher body string, got {other:?}"),
        };

        assert_eq!(ctx.read_string(body).as_deref(), Some("abc"));
    }

    fn re5_test_byte_buffer(ctx: &mut MockNativeContext, bytes: &[u8]) -> ObjectRef {
        let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
        for (i, b) in bytes.iter().copied().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
        }
        let bb = alloc_concurrent_synthetic(ctx, "java/nio/HeapByteBuffer", 5);
        ctx.set_field(bb, 0, Value::Object(Some(arr)));
        ctx.set_field(bb, 1, Value::Int(0));
        ctx.set_field(bb, 2, Value::Int(bytes.len() as i32));
        ctx.set_field(bb, 3, Value::Int(bytes.len() as i32));
        ctx.set_field(bb, 4, Value::Int(-1));
        bb
    }

    fn re5_scripted_publisher_subscribe(
        ctx: &mut MockNativeContext,
        _receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        if method_name == "request" && descriptor == "(J)V" {
            return Some(Ok(None));
        }
        if method_name != "subscribe" || descriptor != "(Ljava/util/concurrent/Flow$Subscriber;)V" {
            return None;
        }
        let subscriber = match args.first().copied() {
            Some(Value::Object(Some(s))) => s,
            _ => return Some(Ok(None)),
        };
        let subscription =
            alloc_concurrent_synthetic(ctx, "java/util/concurrent/Flow$Subscription", 2);
        if let Err(e) = re5_body_collector_on_subscribe(
            ctx,
            &[
                Value::Object(Some(subscriber)),
                Value::Object(Some(subscription)),
            ],
        ) {
            return Some(Err(e));
        }
        let bb = re5_test_byte_buffer(ctx, b"streamed-body");
        if let Err(e) = re5_body_collector_on_next(
            ctx,
            &[Value::Object(Some(subscriber)), Value::Object(Some(bb))],
        ) {
            return Some(Err(e));
        }
        Some(re5_body_collector_on_complete(
            ctx,
            &[Value::Object(Some(subscriber))],
        ))
    }

    #[test]
    fn re5_body_publishers_from_publisher_keeps_flow_publisher() {
        let mut registry = NativeMethodRegistry::new();
        register_re5_http_client(&mut registry);
        let native = registry
            .find(
                "java/net/http/HttpRequest$BodyPublishers",
                "fromPublisher",
                "(Ljava/util/concurrent/Flow$Publisher;)Ljava/net/http/HttpRequest$BodyPublisher;",
            )
            .expect("fromPublisher native is registered");

        let mut ctx = MockNativeContext::new();
        let publisher = alloc_concurrent_synthetic(&mut ctx, "test/SynchronousPublisher", 0);
        let body_publisher = match native(&mut ctx, &[Value::Object(Some(publisher))]).unwrap() {
            Some(Value::Object(Some(body_publisher))) => body_publisher,
            other => panic!("expected BodyPublisher object, got {other:?}"),
        };

        assert!(matches!(
            ctx.get_field(body_publisher, 0),
            Value::Object(Some(o)) if o == publisher
        ));
    }

    #[test]
    fn re5_request_body_bytes_drives_from_publisher_bytebuffers() {
        let mut ctx = MockNativeContext::new();
        ctx.set_invoke_virtual_hook(re5_scripted_publisher_subscribe);
        let publisher = alloc_concurrent_synthetic(&mut ctx, "test/SynchronousPublisher", 0);

        let body = re5_request_body_bytes(&mut ctx, Value::Object(Some(publisher))).unwrap();

        assert_eq!(body, b"streamed-body");
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

    // VULN-FIX [nb-net-phase-e] regression: an oversized advertised body must be
    // rejected with 413 and NOT allocated. We set a tiny per-process unlikely
    // value by exploiting the default cap (8 MiB) being far above 10 bytes while
    // we claim a body larger than the cap via Content-Length.
    #[test]
    fn re10_oversized_content_length_rejected_with_413() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Advertise a body well beyond the 8 MiB default cap; do NOT actually
        // send it (the server must refuse before reading the body).
        let huge = http_max_request_body() + 1;
        let handle = std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(
                format!("POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: {huge}\r\n\r\n").as_bytes(),
            )
            .unwrap();
            // Read back whatever the server responds with.
            let mut resp = Vec::new();
            let _ = c.read_to_end(&mut resp);
            resp
        });
        let (stream, _) = listener.accept().unwrap();
        // parse must reject (None) without reading the huge body.
        assert!(parse_http_request(stream).is_none());
        let resp = handle.join().unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(
            text.starts_with("HTTP/1.1 413"),
            "expected 413 response, got: {text}"
        );
    }

    // VULN-FIX [nb-net-phase-e] regression: conflicting duplicate Content-Length
    // headers (request-smuggling vector) must be rejected with 400 rather than
    // last-wins.
    #[test]
    fn re10_conflicting_content_length_rejected_with_400() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(
                b"POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\nContent-Length: 5\r\n\r\nabc",
            )
            .unwrap();
            let mut resp = Vec::new();
            let _ = c.read_to_end(&mut resp);
            resp
        });
        let (stream, _) = listener.accept().unwrap();
        assert!(parse_http_request(stream).is_none());
        let resp = handle.join().unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(
            text.starts_with("HTTP/1.1 400"),
            "expected 400 response, got: {text}"
        );
    }

    // VULN-FIX [nb-net-phase-e] regression: a small, well-formed body within the
    // cap is still accepted unchanged.
    #[test]
    fn re10_small_body_within_cap_accepted() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(b"POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello")
                .unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        let req = parse_http_request(stream).unwrap();
        assert_eq!(req.body, b"hello");
    }

    // VULN-FIX [nb-net-phase-e] regression: a chunked request body (no
    // Content-Length) must be fully decoded, not silently ignored. Without the
    // fix the parser framed solely by Content-Length (defaulting to 0), read
    // zero body bytes, and left the chunked payload unread → keep-alive desync.
    #[test]
    fn re10_chunked_request_body_is_fully_read() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(
                b"POST /x HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n\
                  5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n",
            )
            .unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        let req = parse_http_request(stream).unwrap();
        assert_eq!(req.body, b"helloworld");
    }

    // VULN-FIX [nb-net-phase-e] regression: a chunked body that arrives in
    // multiple TCP reads (split across the header boundary and between chunks)
    // must still be reassembled completely.
    #[test]
    fn re10_chunked_request_body_split_across_reads() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            // Header + first partial chunk, then the rest in a second write.
            c.write_all(
                b"POST /x HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc",
            )
            .unwrap();
            c.flush().unwrap();
            std::thread::sleep(Duration::from_millis(50));
            c.write_all(b"\r\n4\r\ndefg\r\n0\r\n\r\n").unwrap();
            c.flush().unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        let req = parse_http_request(stream).unwrap();
        assert_eq!(req.body, b"abcdefg");
    }

    // VULN-FIX [nb-net-phase-e] regression: Transfer-Encoding AND Content-Length
    // together is a request-smuggling vector (RFC 7230 §3.3.3) and must be
    // rejected with 400 rather than framed by either header.
    #[test]
    fn re10_te_and_content_length_together_rejected_with_400() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(
                b"POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\
                  Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            )
            .unwrap();
            let mut resp = Vec::new();
            let _ = c.read_to_end(&mut resp);
            resp
        });
        let (stream, _) = listener.accept().unwrap();
        assert!(parse_http_request(stream).is_none());
        let resp = handle.join().unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(
            text.starts_with("HTTP/1.1 400"),
            "expected 400 response, got: {text}"
        );
    }

    // VULN-FIX [nb-net-phase-e] regression: an unknown / unsupported transfer
    // coding (here gzip, which we cannot de-frame) must be rejected with 400
    // instead of guessing at the body boundary.
    #[test]
    fn re10_unknown_transfer_encoding_rejected_with_400() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(b"POST /x HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: gzip\r\n\r\n")
                .unwrap();
            let mut resp = Vec::new();
            let _ = c.read_to_end(&mut resp);
            resp
        });
        let (stream, _) = listener.accept().unwrap();
        assert!(parse_http_request(stream).is_none());
        let resp = handle.join().unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(
            text.starts_with("HTTP/1.1 400"),
            "expected 400 response, got: {text}"
        );
    }

    // VULN-FIX [nb-net-phase-e] regression: `chunked` must be the FINAL coding
    // to frame the body (RFC 7230 §3.3.1). A list ending in a non-chunked
    // coding (e.g. `chunked, gzip`) is unsafe to de-frame → 400.
    #[test]
    fn re10_chunked_not_last_rejected_with_400() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(b"POST /x HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked, gzip\r\n\r\n")
                .unwrap();
            let mut resp = Vec::new();
            let _ = c.read_to_end(&mut resp);
            resp
        });
        let (stream, _) = listener.accept().unwrap();
        assert!(parse_http_request(stream).is_none());
        let resp = handle.join().unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(
            text.starts_with("HTTP/1.1 400"),
            "expected 400 response, got: {text}"
        );
    }

    // VULN-FIX [nb-net-phase-e] regression: a legitimate `identity` coding
    // followed by `chunked` (the only non-chunked coding we accept) still frames
    // by chunked and decodes the body.
    #[test]
    fn re10_identity_then_chunked_is_accepted() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(
                b"POST /x HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: identity, chunked\r\n\r\n\
                  2\r\nhi\r\n0\r\n\r\n",
            )
            .unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        let req = parse_http_request(stream).unwrap();
        assert_eq!(req.body, b"hi");
    }

    #[test]
    fn re6_ssl_context_session_accessors_are_registered() {
        let mut registry = NativeMethodRegistry::new();
        register_re6_ssl_context(&mut registry);
        for method in ["getClientSessionContext", "getServerSessionContext"] {
            assert!(
                registry
                    .find(
                        "javax/net/ssl/SSLContext",
                        method,
                        "()Ljavax/net/ssl/SSLSessionContext;",
                    )
                    .is_some(),
                "missing {method} native"
            );
        }
    }

    #[test]
    fn re5_real_jdk_http_client_builder_fluent_methods_are_registered() {
        let mut registry = NativeMethodRegistry::new();
        register_re5_http_client(&mut registry);
        for (method, descriptor) in [
            (
                "version",
                "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpClient$Builder;",
            ),
            ("priority", "(I)Ljava/net/http/HttpClient$Builder;"),
            (
                "executor",
                "(Ljava/util/concurrent/Executor;)Ljava/net/http/HttpClient$Builder;",
            ),
            (
                "cookieHandler",
                "(Ljava/net/CookieHandler;)Ljava/net/http/HttpClient$Builder;",
            ),
            (
                "proxy",
                "(Ljava/net/ProxySelector;)Ljava/net/http/HttpClient$Builder;",
            ),
            (
                "authenticator",
                "(Ljava/net/Authenticator;)Ljava/net/http/HttpClient$Builder;",
            ),
            (
                "sslContext",
                "(Ljavax/net/ssl/SSLContext;)Ljava/net/http/HttpClient$Builder;",
            ),
            (
                "sslParameters",
                "(Ljavax/net/ssl/SSLParameters;)Ljava/net/http/HttpClient$Builder;",
            ),
        ] {
            assert!(
                registry
                    .find("java/net/http/HttpClient$Builder", method, descriptor)
                    .is_some(),
                "missing {method}{descriptor}"
            );
        }
    }

    #[test]
    fn re10_http_server_executor_bridges_are_concrete_receiver_registrations() {
        let mut registry = NativeMethodRegistry::new();
        register_re10_http_server(&mut registry);
        for class in [
            "com/sun/net/httpserver/HttpServer",
            "sun/net/httpserver/HttpServerImpl",
        ] {
            assert!(
                registry
                    .find(class, "setExecutor", "(Ljava/util/concurrent/Executor;)V")
                    .is_some(),
                "missing {class}.setExecutor bridge"
            );
            assert!(
                registry
                    .find(class, "getExecutor", "()Ljava/util/concurrent/Executor;")
                    .is_some(),
                "missing {class}.getExecutor bridge"
            );
        }
    }
}
