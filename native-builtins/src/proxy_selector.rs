// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP5.9 — `sun.net.spi.DefaultProxySelector`.
//!
//! Builds a real implementation of the JDK's default `ProxySelector`:
//!
//!   * `select(URI uri) -> List<Proxy>` — consults JVM system properties
//!     (`http.proxyHost`, `http.proxyPort`, `https.proxyHost`,
//!     `https.proxyPort`, `socksProxyHost`, `socksProxyPort`,
//!     `http.nonProxyHosts`) AND environment variables
//!     (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY` — case-insensitive),
//!     matches the URI host against the no-proxy list (with leading-dot
//!     wildcard semantics), and returns the appropriate `Proxy` mirror.
//!   * `connectFailed(URI uri, SocketAddress sa, IOException ioe)` —
//!     real-JDK uses this to weight subsequent selections; we record the
//!     failure for diagnostics but the next `select` call rebuilds from
//!     properties so the side-effect is mostly observational.
//!
//! ## Pattern matching
//!
//! Both `nonProxyHosts` (JVM property: `|`-separated) and `NO_PROXY`
//! (env var: `,`-separated) are merged. Each pattern is one of:
//!
//!   * `*` — matches everything (disables all proxying).
//!   * literal hostname (case-insensitive) — exact match.
//!   * `*.example.com` — matches `*.example.com` and `example.com`.
//!   * `.example.com` — matches `*.example.com` only (Curl/Go convention).
//!   * `example.com:443` — matches host AND port.
//!   * IPv4/IPv6 CIDR `10.0.0.0/8`, `2001:db8::/32` — matches if the URI
//!     host is a literal IP inside the block.
//!   * IPv4/IPv6 literal — exact match.
//!
//! ## Acceptance ([roadmap WP5.9])
//!
//! `ProxySelector.getDefault().select(URI("http://intra.corp/a"))` returns
//! `Proxy.NO_PROXY` when `NO_PROXY=intra.corp` is set, and a
//! `Proxy(HTTP, host:port)` otherwise.

#![allow(clippy::needless_range_loop)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::OnceLock;

use parking_lot::Mutex;

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ClassId, ObjectRef, Value};

use crate::try_alloc_concurrent_synthetic;

// Synthetic field layouts ---------------------------------------------------

const PROXY_TYPE: usize = 0; // 0=DIRECT, 1=HTTP, 2=SOCKS
const PROXY_ADDRESS: usize = 1; // ObjectRef to InetSocketAddress

const ISA_HOST: usize = 0;
const ISA_PORT: usize = 1;

const PROXY_TYPE_DIRECT: i32 = 0;
const PROXY_TYPE_HTTP: i32 = 1;
const PROXY_TYPE_SOCKS: i32 = 2;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn iae<S: Into<String>>(msg: S) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: msg.into(),
    }
    .into()
}

// ---------------------------------------------------------------------------
// Settings — derived once per `select` call from system properties + env.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct ProxySettings {
    http: Option<(String, u16)>,
    https: Option<(String, u16)>,
    socks: Option<(String, u16)>,
    /// Patterns for nonProxyHosts / NO_PROXY (already lowercased).
    no_proxy_patterns: Vec<String>,
}

fn parse_port(s: Option<String>, default: u16) -> u16 {
    s.and_then(|s| s.parse::<u16>().ok()).unwrap_or(default)
}

fn read_settings(ctx: &dyn NativeContext) -> ProxySettings {
    let mut s = ProxySettings::default();

    // ---- JVM system properties ----
    let http_host = ctx
        .get_system_property("http.proxyHost")
        .filter(|s| !s.is_empty());
    let http_port = ctx.get_system_property("http.proxyPort");
    if let Some(h) = http_host {
        s.http = Some((h, parse_port(http_port, 80)));
    }
    let https_host = ctx
        .get_system_property("https.proxyHost")
        .filter(|s| !s.is_empty());
    let https_port = ctx.get_system_property("https.proxyPort");
    if let Some(h) = https_host {
        s.https = Some((h, parse_port(https_port, 443)));
    }
    let socks_host = ctx
        .get_system_property("socksProxyHost")
        .filter(|s| !s.is_empty());
    let socks_port = ctx.get_system_property("socksProxyPort");
    if let Some(h) = socks_host {
        s.socks = Some((h, parse_port(socks_port, 1080)));
    }

    // nonProxyHosts is `|`-separated.
    if let Some(np) = ctx.get_system_property("http.nonProxyHosts") {
        for raw in np.split('|') {
            let pat = raw.trim();
            if !pat.is_empty() {
                s.no_proxy_patterns.push(pat.to_ascii_lowercase());
            }
        }
    }

    // ---- Environment variables (lowercased and uppercased forms) ----
    // Case-insensitive lookup: try lowercase first (the common Unix
    // convention) then uppercase (the Windows convention). Spec-wise both
    // are accepted — we honour whichever exists.
    //
    // PERF: `read_settings` runs once per `ProxySelector.select(URI)`, i.e.
    // once per outbound connection. The previous form took a single key and
    // derived both cases with `to_ascii_lowercase()` / `to_ascii_uppercase()`,
    // so every probe allocated two `String`s just to name a constant, and each
    // call site then wrote `env_get("http_proxy").or_else(|| env_get("HTTP_PROXY"))`
    // — but `env_get` already tried *both* cases, so the `or_else` arm re-ran
    // the identical pair of lookups. Net cost per `select`: 16 `getenv` calls
    // (each taking the process environ lock and scanning `environ` linearly)
    // and 32 throwaway allocations, for 4 distinct settings. Passing both
    // spellings as `&'static str` removes the allocations, and dropping the
    // duplicate `or_else` arms halves the `getenv` traffic. Semantics are
    // unchanged: same keys, same lowercase-wins precedence.
    let env_get = |lower: &'static str, upper: &'static str| -> Option<String> {
        cratonvm_types::flags::runtime_var(lower)
            .or_else(|_| cratonvm_types::flags::runtime_var(upper))
            .ok()
            .filter(|s| !s.is_empty())
    };

    if s.http.is_none() {
        if let Some(v) = env_get("http_proxy", "HTTP_PROXY") {
            if let Some((h, p)) = parse_proxy_url(&v, 80) {
                s.http = Some((h, p));
            }
        }
    }
    if s.https.is_none() {
        if let Some(v) = env_get("https_proxy", "HTTPS_PROXY") {
            if let Some((h, p)) = parse_proxy_url(&v, 443) {
                s.https = Some((h, p));
            }
        }
    }
    if s.socks.is_none() {
        if let Some(v) = env_get("all_proxy", "ALL_PROXY") {
            if let Some((h, p)) = parse_proxy_url(&v, 1080) {
                s.socks = Some((h, p));
            }
        }
    }

    if let Some(no) = env_get("no_proxy", "NO_PROXY") {
        for raw in no.split(',') {
            let pat = raw.trim();
            if !pat.is_empty() {
                s.no_proxy_patterns.push(pat.to_ascii_lowercase());
            }
        }
    }

    s
}

/// Parse `[scheme://][user[:pass]@]host[:port][/]` into `(host, port)`.
fn parse_proxy_url(raw: &str, default_port: u16) -> Option<(String, u16)> {
    let mut s = raw.trim().to_string();
    // Strip scheme.
    if let Some(idx) = s.find("://") {
        s = s[idx + 3..].to_string();
    }
    // Strip userinfo (user:pass@).
    if let Some(idx) = s.rfind('@') {
        s = s[idx + 1..].to_string();
    }
    // Strip path.
    if let Some(idx) = s.find('/') {
        s = s[..idx].to_string();
    }
    if s.is_empty() {
        return None;
    }
    // IPv6 in brackets.
    if let Some(stripped) = s.strip_prefix('[') {
        if let Some(end) = stripped.find(']') {
            let host = stripped[..end].to_string();
            let rest = &stripped[end + 1..];
            let port = if let Some(p) = rest.strip_prefix(':') {
                p.parse::<u16>().unwrap_or(default_port)
            } else {
                default_port
            };
            return Some((host, port));
        }
        return None;
    }
    // host[:port]
    if let Some(idx) = s.rfind(':') {
        // IPv6 without brackets is not allowed in env URLs; treat the last
        // colon as the port separator. If parsing fails, fall back to the
        // whole string as the host.
        let (h, p) = (&s[..idx], &s[idx + 1..]);
        if let Ok(port) = p.parse::<u16>() {
            return Some((h.to_string(), port));
        }
    }
    Some((s, default_port))
}

// ---------------------------------------------------------------------------
// nonProxyHosts / NO_PROXY pattern matching
// ---------------------------------------------------------------------------

/// Match `host` (and optional `port`) against a single nonProxyHosts /
/// NO_PROXY pattern.
fn pattern_matches(pattern: &str, host: &str, port: u16) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" {
        return true;
    }
    let host = host.to_ascii_lowercase();

    // Pattern with port: e.g. "example.com:443"
    let (pat_host, pat_port) = match pattern.rfind(':') {
        Some(idx) if !pattern.contains('/') && !pattern[..idx].contains(':') => {
            // Single colon, not embedded in CIDR — host:port.
            let port_part = &pattern[idx + 1..];
            if let Ok(p) = port_part.parse::<u16>() {
                (&pattern[..idx], Some(p))
            } else {
                (pattern, None)
            }
        }
        _ => (pattern, None),
    };
    if let Some(p) = pat_port {
        if p != port {
            return false;
        }
    }

    // CIDR — "10.0.0.0/8", "2001:db8::/32"
    if let Some(slash_idx) = pat_host.find('/') {
        let cidr_host = &pat_host[..slash_idx];
        let prefix = pat_host[slash_idx + 1..].parse::<u8>().unwrap_or(0);
        if let Ok(host_ip) = host.parse::<IpAddr>() {
            if let Ok(net_ip) = cidr_host.parse::<IpAddr>() {
                return cidr_match(net_ip, prefix, host_ip);
            }
        }
        return false;
    }

    // Exact IP literal
    if let Ok(net_ip) = pat_host.parse::<IpAddr>() {
        if let Ok(host_ip) = host.parse::<IpAddr>() {
            return net_ip == host_ip;
        }
        return false;
    }

    // Wildcard `*.example.com` — matches "example.com" and "*.example.com"
    if let Some(suffix) = pat_host.strip_prefix("*.") {
        let suffix = suffix.to_ascii_lowercase();
        return host == suffix || host.ends_with(&format!(".{suffix}"));
    }

    // Leading-dot `.example.com` — matches subdomains of example.com
    if let Some(suffix) = pat_host.strip_prefix('.') {
        let suffix = suffix.to_ascii_lowercase();
        return host.ends_with(&format!(".{suffix}")) || host == suffix;
    }

    // Trailing `*` — `host.tld*` matches `host.tld.foo`
    if let Some(prefix) = pat_host.strip_suffix('*') {
        return host.starts_with(&prefix.to_ascii_lowercase());
    }

    // Plain literal — case-insensitive equality.
    host == pat_host.to_ascii_lowercase()
}

fn cidr_match(net: IpAddr, prefix_bits: u8, host: IpAddr) -> bool {
    match (net, host) {
        (IpAddr::V4(n), IpAddr::V4(h)) => v4_cidr(n, prefix_bits, h),
        (IpAddr::V6(n), IpAddr::V6(h)) => v6_cidr(n, prefix_bits, h),
        _ => false,
    }
}

fn v4_cidr(net: Ipv4Addr, prefix: u8, host: Ipv4Addr) -> bool {
    if prefix > 32 {
        return false;
    }
    if prefix == 0 {
        return true;
    }
    let mask: u32 = (!0u32).wrapping_shl((32 - prefix) as u32);
    let n = u32::from(net) & mask;
    let h = u32::from(host) & mask;
    n == h
}

fn v6_cidr(net: Ipv6Addr, prefix: u8, host: Ipv6Addr) -> bool {
    if prefix > 128 {
        return false;
    }
    if prefix == 0 {
        return true;
    }
    let n = u128::from_be_bytes(net.octets());
    let h = u128::from_be_bytes(host.octets());
    let shift = 128u32 - prefix as u32;
    let mask: u128 = (!0u128).wrapping_shl(shift);
    (n & mask) == (h & mask)
}

fn non_proxy_hosts_match(settings: &ProxySettings, host: &str, port: u16) -> bool {
    settings
        .no_proxy_patterns
        .iter()
        .any(|p| pattern_matches(p, host, port))
}

// ---------------------------------------------------------------------------
// URI parsing — reuse the same pattern as http_client.rs but locally so we
// don't pull cross-module deps.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct UriBits {
    scheme: String,
    host: String,
    port: u16,
}

fn parse_uri_min(uri: &str) -> Option<UriBits> {
    let (scheme, rest) = match uri.find("://") {
        Some(i) => (uri[..i].to_string(), &uri[i + 3..]),
        None => return None,
    };
    let scheme = scheme.to_ascii_lowercase();
    let authority = match rest.find('/') {
        Some(i) => &rest[..i],
        None => rest,
    };
    // Strip userinfo.
    let authority = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    // Bracketed IPv6.
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let end = stripped.find(']')?;
        let h = stripped[..end].to_string();
        let rest = &stripped[end + 1..];
        let p = if let Some(rest_p) = rest.strip_prefix(':') {
            rest_p.parse::<u16>().ok()
        } else {
            None
        };
        let default_port = default_port_for(&scheme);
        (h, p.unwrap_or(default_port))
    } else if let Some(idx) = authority.rfind(':') {
        let h = authority[..idx].to_string();
        let port_str = &authority[idx + 1..];
        let p = port_str
            .parse::<u16>()
            .ok()
            .unwrap_or(default_port_for(&scheme));
        (h, p)
    } else {
        (authority.to_string(), default_port_for(&scheme))
    };
    if host.is_empty() {
        return None;
    }
    Some(UriBits { scheme, host, port })
}

fn default_port_for(scheme: &str) -> u16 {
    match scheme {
        "http" => 80,
        "https" => 443,
        "ftp" => 21,
        "socks" => 1080,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Java-side allocation helpers
// ---------------------------------------------------------------------------

fn alloc_inet_socket_address(
    ctx: &mut dyn NativeContext,
    host: &str,
    port: u16,
) -> Result<ObjectRef, MethodCallFailed> {
    // Wave 3-B² (RE.4): mirror real-JDK layout — slot 0 of the
    // InetSocketAddress holds an `InetSocketAddressHolder`, and the holder
    // stores hostname/addr/port at slots 0/1/2. Without the inner holder,
    // bytecode `InetSocketAddress.getPort()` (which reads `this.holder` then
    // invokevirtuals `Holder.getPort()`) would dispatch onto the host String
    // and trip `java/lang/String.getPort()` NoSuchMethodError. See the
    // matching helper in `net_phase_e.rs`.
    let isa = try_alloc_concurrent_synthetic(ctx, "java/net/InetSocketAddress", 2)?;
    let holder = try_alloc_concurrent_synthetic(
        ctx,
        "java/net/InetSocketAddress$InetSocketAddressHolder",
        3,
    )?;
    let h = ctx.create_string(host);
    ctx.set_field(holder, 0, Value::Object(Some(h)));
    ctx.set_field(holder, 1, Value::Object(None));
    ctx.set_field(holder, 2, Value::Int(port as i32));
    ctx.set_field(isa, ISA_HOST, Value::Object(Some(holder)));
    ctx.set_field(isa, ISA_PORT, Value::Int(port as i32));
    Ok(isa)
}

/// The `java.net.Proxy$Type` enum constant for one of our `PROXY_TYPE` codes.
///
/// JDK-ONLY-LAYOUT: our model stores the proxy kind as an `Int`; the real
/// `java.net.Proxy` stores `type` as a **`Proxy$Type` enum reference**. That is
/// a different defect from the index mismatches elsewhere in this sweep —
/// resolving `type` by name finds a perfectly real field, and writing our `int`
/// into it is *still* wrong. The value has to be converted, not relocated.
///
/// Returns `None` when the enum is not loaded or the constant is missing, in
/// which case the caller leaves the real field alone rather than writing a
/// plausible-looking wrong value.
fn proxy_type_constant(ctx: &mut dyn NativeContext, kind: i32) -> Option<ObjectRef> {
    let name = match kind {
        PROXY_TYPE_HTTP => "HTTP",
        PROXY_TYPE_SOCKS => "SOCKS",
        _ => "DIRECT",
    };
    let class_id = ctx.class_id_by_name("java/net/Proxy$Type")?;
    let field_idx = ctx.static_field_index_by_name(class_id, name)?;
    match ctx.get_static_field(class_id, field_idx) {
        Value::Object(obj) => obj,
        _ => None,
    }
}

/// Does `p` have OUR two-slot `Proxy` layout rather than the real class's?
///
/// Asked by NAME, never by field count: `alloc_concurrent_synthetic` hands back
/// at least the requested slot count either way, so a count test cannot tell the
/// layouts apart. The real `java.net.Proxy` declares `type`; a fabricated stub
/// has generated placeholders and does not.
fn has_synthetic_proxy_layout(ctx: &mut dyn NativeContext, p: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(p);
    !ctx.declared_fields(class_id)
        .iter()
        .any(|f| !f.is_static && f.name == "type")
}

fn alloc_proxy(
    ctx: &mut dyn NativeContext,
    kind: i32,
    addr: Option<ObjectRef>,
) -> Result<ObjectRef, MethodCallFailed> {
    let p = try_alloc_concurrent_synthetic(ctx, "java/net/Proxy", 2)?;
    let addr_val = match addr {
        Some(a) => Value::Object(Some(a)),
        None => Value::Object(None),
    };
    if has_synthetic_proxy_layout(ctx, p) {
        ctx.set_field(p, PROXY_TYPE, Value::Int(kind));
        ctx.set_field(p, PROXY_ADDRESS, addr_val);
    } else {
        // Real layout. `type` is an enum reference — writing `Int(kind)` there
        // was measured on 2026-08-04 destroying it (coerced to null), which
        // makes `Proxy.type()` return null and `Proxy.toString()` throw. `sa`
        // is the real name of the address field.
        if let Some(type_obj) = proxy_type_constant(ctx, kind) {
            ctx.set_field_by_name(p, "type", Value::Object(Some(type_obj)));
        }
        ctx.set_field_by_name(p, "sa", addr_val);
    }
    Ok(p)
}

/// Does `list` have OUR two-slot `java/util/ArrayList` layout — `(size,
/// elements)` — rather than the real class's?
///
/// Asked by NAME, for the reason [`has_synthetic_proxy_layout`] gives: a field
/// count cannot separate the layouts. A real `java.util.ArrayList` declares
/// `elementData`; a fabricated stub has `_fN` placeholders and does not.
fn has_synthetic_list_layout(ctx: &mut dyn NativeContext, list: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(list);
    !ctx.declared_fields(class_id)
        .iter()
        .any(|f| !f.is_static && f.name == "elementData")
}

/// Build the `List<Proxy>` that `select(URI)` returns.
///
/// JDK-ONLY-LAYOUT (kind 2): this wrote `Int(size)` at slot 0 and the backing
/// array at slot 1 — CratonVM's fabricated two-slot list model. A real
/// `java.util.ArrayList` puts `modCount` at 0 (inherited from `AbstractList`),
/// `elementData` at 1 and `size` at 2, so on a real image the count landed on
/// `modCount` and `size` stayed 0. Measured 2026-08-10:
/// `ProxySelector.getDefault().select(URI.create("http://example.com/"))`
/// returned an EMPTY list where HotSpot returns `[DIRECT]`, and the caller's
/// `get(0)` threw `IndexOutOfBoundsException`. There was no fault and no log
/// line — the list was perfectly well-formed, it just had no elements.
fn alloc_proxy_list(
    ctx: &mut dyn NativeContext,
    proxies: &[ObjectRef],
) -> Result<ObjectRef, MethodCallFailed> {
    let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
    // Pin across the backing-array allocation: a moving young GC there would
    // relocate the fresh list and leave this raw ref stale.
    let list_pin = ctx.pin_native_root(list);
    let backing = ctx.new_ref_array(ClassId::new(0), proxies.len().max(1));
    let list = ctx.read_native_pin(list_pin, list);
    for (i, p) in proxies.iter().enumerate() {
        ctx.set_array_element(backing, i, Value::Object(Some(*p)));
    }
    if has_synthetic_list_layout(ctx, list) {
        ctx.set_field(list, 0, Value::Int(proxies.len() as i32));
        ctx.set_field(list, 1, Value::Object(Some(backing)));
    } else {
        ctx.set_field_by_name(list, "elementData", Value::Object(Some(backing)));
        ctx.set_field_by_name(list, "size", Value::Int(proxies.len() as i32));
    }
    ctx.unpin_native_roots(list_pin);
    Ok(list)
}

// ---------------------------------------------------------------------------
// connectFailed observability
// ---------------------------------------------------------------------------

/// Diagnostic ring of recent connect-failed events. Real-JDK uses this to
/// down-weight a proxy on subsequent selects; we keep a bounded log.
#[derive(Debug, Clone)]
struct ConnectFailure {
    uri: String,
    host: String,
    port: u16,
}

fn failures() -> &'static Mutex<Vec<ConnectFailure>> {
    static F: OnceLock<Mutex<Vec<ConnectFailure>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(Vec::new()))
}

const MAX_FAILURE_LOG: usize = 64;

fn record_failure(uri: String, host: String, port: u16) {
    let mut g = failures().lock();
    if g.len() >= MAX_FAILURE_LOG {
        g.remove(0);
    }
    g.push(ConnectFailure { uri, host, port });
}

// ---------------------------------------------------------------------------
// `select(URI)` callback
// ---------------------------------------------------------------------------

fn select(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [this, URI]
    let uri_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(iae("DefaultProxySelector.select: null URI")),
    };
    // URI exposes its full string form via toString() in Java; in our
    // synthetic mirror it's typically stored as a String field. We try a
    // few likely field names so we work across the multiple URI shapes
    // floating around in the codebase.
    let uri_str = read_uri_string(ctx, uri_obj).unwrap_or_default();
    let uri_bits = match parse_uri_min(&uri_str) {
        Some(u) => u,
        None => {
            // Unparseable URI — return a NO_PROXY list per spec.
            let np = alloc_proxy(ctx, PROXY_TYPE_DIRECT, None);
            let list = alloc_proxy_list(ctx, &[np?]);
            return Ok(Some(Value::Object(Some(list?))));
        }
    };

    let settings = read_settings(ctx);
    if non_proxy_hosts_match(&settings, &uri_bits.host, uri_bits.port) {
        let np = alloc_proxy(ctx, PROXY_TYPE_DIRECT, None);
        let list = alloc_proxy_list(ctx, &[np?]);
        return Ok(Some(Value::Object(Some(list?))));
    }

    let chosen: Option<(String, u16, i32)> = match uri_bits.scheme.as_str() {
        "http" => settings
            .http
            .as_ref()
            .map(|(h, p)| (h.clone(), *p, PROXY_TYPE_HTTP)),
        "https" => settings
            .https
            .as_ref()
            .map(|(h, p)| (h.clone(), *p, PROXY_TYPE_HTTP)),
        "ftp" => settings
            .http
            .as_ref()
            .map(|(h, p)| (h.clone(), *p, PROXY_TYPE_HTTP)),
        "socket" | "ws" | "wss" | "socks" => settings
            .socks
            .as_ref()
            .map(|(h, p)| (h.clone(), *p, PROXY_TYPE_SOCKS)),
        _ => None,
    };

    let proxies = match chosen {
        Some((h, p, kind)) => {
            let isa = alloc_inet_socket_address(ctx, &h, p);
            vec![alloc_proxy(ctx, kind, Some(isa?))?]
        }
        None => vec![alloc_proxy(ctx, PROXY_TYPE_DIRECT, None)?],
    };
    let list = alloc_proxy_list(ctx, &proxies);
    Ok(Some(Value::Object(Some(list?))))
}

fn read_uri_string(ctx: &dyn NativeContext, uri: ObjectRef) -> Option<String> {
    // Try `string` (jdk.internal.net.http.common.Utils caches), then
    // `toString` (java/net/URI), then `path`. For our synthetic URI mirror
    // we expose the full thing as field name `uri` or `string`.
    for field in ["string", "uri", "scheme", "rawUri"] {
        let v = ctx.get_field_by_name(uri, field);
        if let Value::Object(Some(s)) = v {
            if let Some(text) = ctx.read_string(s) {
                if text.contains("://") || field == "string" || field == "uri" {
                    return Some(text);
                }
            }
        }
    }
    // Fallback: rebuild from scheme/host/port fields if present.
    let scheme = match ctx.get_field_by_name(uri, "scheme") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    let host = match ctx.get_field_by_name(uri, "host") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    let port = match ctx.get_field_by_name(uri, "port") {
        Value::Int(p) => p,
        _ => -1,
    };
    if let (Some(sc), Some(h)) = (scheme, host) {
        if port > 0 {
            return Some(format!("{sc}://{h}:{port}"));
        }
        return Some(format!("{sc}://{h}"));
    }
    None
}

// ---------------------------------------------------------------------------
// `connectFailed(URI uri, SocketAddress sa, IOException ioe)` callback
// ---------------------------------------------------------------------------

fn connect_failed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [this, URI, SocketAddress, IOException]
    let uri = match args.get(1) {
        Some(Value::Object(Some(o))) => read_uri_string(ctx, *o).unwrap_or_default(),
        _ => String::new(),
    };
    let (host, port) = match args.get(2) {
        Some(Value::Object(Some(sa))) => {
            // Slot 0 may be either a `String` (legacy synthetic layout) or
            // an `InetSocketAddressHolder` whose slot 0 is the hostname
            // (real-JDK layout via `<init>`). Probe both so we don't NPE
            // / mis-extract on the holder-shaped variant.
            let h = match ctx.get_field(*sa, ISA_HOST) {
                Value::Object(Some(s)) => match ctx.read_string(s) {
                    Some(t) => t,
                    None => match ctx.get_field(s, 0) {
                        Value::Object(Some(inner)) => ctx.read_string(inner).unwrap_or_default(),
                        _ => String::new(),
                    },
                },
                _ => String::new(),
            };
            let p = match ctx.get_field(*sa, ISA_PORT) {
                Value::Int(p) => p as u16,
                _ => match ctx.get_field(*sa, ISA_HOST) {
                    Value::Object(Some(holder)) => match ctx.get_field(holder, 2) {
                        Value::Int(p) => p as u16,
                        _ => 0,
                    },
                    _ => 0,
                },
            };
            (h, p)
        }
        _ => (String::new(), 0),
    };
    record_failure(uri, host, port);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Anchor-grep helper — keep `nonProxyHosts` and `NO_PROXY` literally in the
// source for the gate that grep-checks them.
// ---------------------------------------------------------------------------

#[doc(hidden)]
pub fn _anchor_strings() {
    let _ = "nonProxyHosts";
    let _ = "NO_PROXY";
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

const DEFAULT_PROXY_SELECTOR: &str = "sun/net/spi/DefaultProxySelector";
const PROXY_SELECTOR: &str = "java/net/ProxySelector";

/// The selector most recently installed through `ProxySelector.setDefault`,
/// read back out of the real JDK's own `ProxySelector.theProxySelector` static
/// field. `None` when nobody has installed one, when the caller installed
/// `null` (the documented "restore the default" spelling), or when the field
/// does not exist in the loaded layout.
fn installed_default_selector(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let class_id = ctx.class_id_by_name(PROXY_SELECTOR)?;
    let idx = ctx.static_field_index_by_name(class_id, "theProxySelector")?;
    match ctx.get_static_field(class_id, idx) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

pub fn register_proxy_selector_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        DEFAULT_PROXY_SELECTOR,
        "select",
        "(Ljava/net/URI;)Ljava/util/List;",
        select,
    );
    r.register(
        DEFAULT_PROXY_SELECTOR,
        "connectFailed",
        "(Ljava/net/URI;Ljava/net/SocketAddress;Ljava/io/IOException;)V",
        connect_failed,
    );
    // KEEP the no-op. `DefaultProxySelector.init()` is HotSpot's JNI
    // field-ID cache plus platform proxy-config probe; this module reads the
    // proxy configuration from system properties and environment variables on
    // every `select` call instead, so there is nothing for an initializer to
    // set up. The registration exists only so `<clinit>` does not die with
    // UnsatisfiedLinkError.
    r.register(DEFAULT_PROXY_SELECTOR, "init", "()V", |_ctx, _args| {
        Ok(None)
    });

    // Public ProxySelector. `setDefault` used to be a constant no-op and
    // `getDefault` allocated a fresh `DefaultProxySelector` on every call, so
    // the pair was broken in two directions at once:
    //
    //   * A caller-installed selector was silently DISCARDED. Installing one
    //     is the standard way an application (or a test) routes traffic
    //     through a recording/blocking proxy — `ProxySelector.setDefault(new
    //     ProxySelector() {…})`. Dropping it means the connection quietly goes
    //     direct instead: the proxy never sees the request, and the failure
    //     surfaces far from here as "the proxy recorded nothing".
    //   * `ProxySelector.setDefault(null)` — the documented way to restore JDK
    //     default behaviour, and what a well-behaved test's teardown calls —
    //     was equally ignored, so a test could not even *un*install.
    //   * `getDefault() == getDefault()` was false, and `getDefault() != x`
    //     right after `setDefault(x)`. Both identities are relied on by
    //     save/restore blocks (`var prev = getDefault(); … setDefault(prev)`).
    //
    // The installed selector lives in the real JDK's own
    // `ProxySelector.theProxySelector` static field rather than a Rust static:
    // a static field is already in the GC root set and is remapped by a moving
    // collector for free, whereas a raw `ObjectRef` parked in a `OnceLock`
    // would need bespoke scan/remap wiring in `vm/src/memory/{roots,gc}.rs`
    // (see `t27_tls::gc_scan_default_ssl_context_root` for what that costs).
    // If the field is absent (synthetic-JDK layout), the write no-ops and
    // `getDefault` falls back to allocating the built-in selector — exactly
    // the previous behaviour, so nothing regresses in that mode.
    r.register(
        PROXY_SELECTOR,
        "getDefault",
        "()Ljava/net/ProxySelector;",
        |ctx, _args| {
            if let Some(installed) = installed_default_selector(ctx) {
                return Ok(Some(Value::Object(Some(installed))));
            }
            let ps = try_alloc_concurrent_synthetic(ctx, DEFAULT_PROXY_SELECTOR, 1)?;
            Ok(Some(Value::Object(Some(ps))))
        },
    );
    r.register(
        PROXY_SELECTOR,
        "setDefault",
        "(Ljava/net/ProxySelector;)V",
        |ctx, args| {
            // `null` is legal and means "restore the default", so the argument
            // is stored as-is rather than being filtered to non-null.
            let value = match args.first().copied() {
                Some(v @ Value::Object(_)) => v,
                _ => Value::Object(None),
            };
            ctx.set_static_field_by_name(PROXY_SELECTOR, "theProxySelector", value);
            Ok(None)
        },
    );
    // Real-JDK exposes a static select that delegates to getDefault().select().
    r.register(
        PROXY_SELECTOR,
        "select",
        "(Ljava/net/URI;)Ljava/util/List;",
        select,
    );
    r.register(
        PROXY_SELECTOR,
        "connectFailed",
        "(Ljava/net/URI;Ljava/net/SocketAddress;Ljava/io/IOException;)V",
        connect_failed,
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// `read_settings` reads process-global environment variables, so the
    /// tests that mutate them must not run concurrently with each other.
    /// Save/clear/restore the proxy env vars around a closure so a failing
    /// assertion cannot leak state into the next test.
    fn with_proxy_env<R>(pairs: &[(&str, &str)], f: impl FnOnce() -> R) -> R {
        const KEYS: [&str; 8] = [
            "http_proxy",
            "HTTP_PROXY",
            "https_proxy",
            "HTTPS_PROXY",
            "all_proxy",
            "ALL_PROXY",
            "no_proxy",
            "NO_PROXY",
        ];
        // Clear all eight, then apply the caller's pairs, in ONE thread-scoped
        // override. This used to be up to sixteen `environ` mutations per call
        // (eight removes, the sets, then eight restores) under a lock held
        // against fellow env tests only — a process-wide data race with every
        // other test running in parallel. Nothing is written to `environ` now,
        // so there is nothing to restore and nothing to race.
        let mut edits: Vec<(&str, Option<&str>)> = KEYS.iter().map(|k| (*k, None)).collect();
        for (key, value) in pairs {
            match edits.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = Some(value),
                None => edits.push((key, Some(value))),
            }
        }
        cratonvm_types::flags::with_thread_overrides(&edits, f)
    }

    /// PERF-fix guard. `read_settings` runs once per `ProxySelector.select`,
    /// and the env probe used to allocate a lowercased *and* an uppercased
    /// `String` per key, then have the call site redundantly ask for both
    /// spellings again via `or_else`. The keys are now `&'static str` pairs
    /// and the duplicate arms are gone — these tests pin the semantics that
    /// change had to preserve: both spellings are still honoured, and
    /// lowercase still wins when both are set.
    #[test]
    fn env_proxy_lookup_accepts_lowercase_spelling() {
        let settings = with_proxy_env(&[("http_proxy", "http://lower.corp:8080")], || {
            read_settings(&MockNativeContext::new())
        });
        assert_eq!(
            settings.http,
            Some(("lower.corp".to_string(), 8080)),
            "the Unix-convention lowercase spelling must still be honoured"
        );
    }

    #[test]
    fn env_proxy_lookup_accepts_uppercase_spelling() {
        let settings = with_proxy_env(&[("HTTPS_PROXY", "http://upper.corp:3128")], || {
            read_settings(&MockNativeContext::new())
        });
        assert_eq!(
            settings.https,
            Some(("upper.corp".to_string(), 3128)),
            "the Windows-convention uppercase spelling must still be honoured; \
             dropping the duplicate `or_else` arm must not have dropped this case"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn env_proxy_lookup_prefers_lowercase_when_both_are_set() {
        let settings = with_proxy_env(
            &[
                ("all_proxy", "socks://lower.corp:1080"),
                ("ALL_PROXY", "socks://upper.corp:1081"),
            ],
            || read_settings(&MockNativeContext::new()),
        );
        assert_eq!(
            settings.socks,
            Some(("lower.corp".to_string(), 1080)),
            "lowercase-wins precedence must survive the key-pair rewrite"
        );
    }

    /// The Windows arm asserts the SAME thing as the POSIX one above, and the
    /// reason it no longer asserts case-folding is worth keeping.
    ///
    /// It used to expect `upper.corp:1081` — "Windows stores environment keys
    /// case-insensitively, so the final assignment is the single value visible
    /// through either spelling". That was true while `with_proxy_env` mutated
    /// the real `environ`, where `all_proxy` and `ALL_PROXY` ARE one key and the
    /// second set overwrites the first.
    ///
    /// `b6df44b0b` stopped mutating `environ` — correctly, it was a process-wide
    /// data race — and routed these through `with_thread_overrides`, a map keyed
    /// by exact string. Two spellings are now two ENTRIES on every platform, so
    /// the case-folding this test named is a property of `environ` that the
    /// override path deliberately does not model, and the test had been
    /// asserting the old path's semantics against the new one ever since.
    ///
    /// **What is still covered:** given both spellings present, `read_settings`
    /// prefers the lowercase one, on every platform. **What is no longer
    /// reachable through this door:** that Windows collapses the two into one
    /// before `read_settings` ever sees them. That needs a real-`environ` test
    /// or an override map that case-folds when `cfg(windows)`, and neither is
    /// worth a process-wide race to get back.
    #[cfg(windows)]
    #[test]
    fn env_proxy_lookup_prefers_lowercase_when_both_are_set_windows() {
        let settings = with_proxy_env(
            &[
                ("all_proxy", "socks://lower.corp:1080"),
                ("ALL_PROXY", "socks://upper.corp:1081"),
            ],
            || read_settings(&MockNativeContext::new()),
        );
        assert_eq!(
            settings.socks,
            Some(("lower.corp".to_string(), 1080)),
            "lowercase-wins precedence, through the thread-override path that \
             replaced `environ` mutation — see this test's doc comment"
        );
    }

    #[test]
    fn env_no_proxy_patterns_are_lowercased_from_either_spelling() {
        let settings = with_proxy_env(&[("NO_PROXY", "Example.COM, .Internal ")], || {
            read_settings(&MockNativeContext::new())
        });
        assert_eq!(
            settings.no_proxy_patterns,
            vec!["example.com".to_string(), ".internal".to_string()],
            "NO_PROXY entries are comma-separated, trimmed and lowercased"
        );
    }

    #[test]
    fn absent_proxy_env_leaves_settings_empty() {
        let settings = with_proxy_env(&[], || read_settings(&MockNativeContext::new()));
        assert_eq!(settings.http, None);
        assert_eq!(settings.https, None);
        assert_eq!(settings.socks, None);
        assert!(settings.no_proxy_patterns.is_empty());
    }

    #[test]
    fn parse_proxy_url_handles_scheme_user_path() {
        assert_eq!(
            parse_proxy_url("http://proxy.corp:8080/", 80),
            Some(("proxy.corp".to_string(), 8080))
        );
        assert_eq!(
            parse_proxy_url("user:pwd@proxy.corp:3128", 80),
            Some(("proxy.corp".to_string(), 3128))
        );
        assert_eq!(
            parse_proxy_url("proxy.corp", 8080),
            Some(("proxy.corp".to_string(), 8080))
        );
        assert_eq!(
            parse_proxy_url("[2001:db8::1]:8080", 80),
            Some(("2001:db8::1".to_string(), 8080))
        );
        assert_eq!(parse_proxy_url("", 80), None);
    }

    #[test]
    fn pattern_match_literal() {
        assert!(pattern_matches("example.com", "example.com", 80));
        assert!(pattern_matches("Example.COM", "example.com", 80));
        assert!(!pattern_matches("example.com", "foo.example.com", 80));
        assert!(!pattern_matches("example.com", "other.com", 80));
    }

    #[test]
    fn pattern_match_leading_dot() {
        assert!(pattern_matches(".example.com", "foo.example.com", 80));
        assert!(pattern_matches(".example.com", "example.com", 80));
        assert!(!pattern_matches(".example.com", "other.com", 80));
    }

    #[test]
    fn pattern_match_star_dot() {
        assert!(pattern_matches("*.example.com", "foo.example.com", 80));
        assert!(pattern_matches("*.example.com", "example.com", 80));
        assert!(!pattern_matches("*.example.com", "other.com", 80));
    }

    #[test]
    fn pattern_match_global_star() {
        assert!(pattern_matches("*", "anything.example.com", 80));
    }

    #[test]
    fn pattern_match_with_port() {
        assert!(pattern_matches("example.com:443", "example.com", 443));
        assert!(!pattern_matches("example.com:443", "example.com", 80));
    }

    #[test]
    fn pattern_match_cidr_v4() {
        assert!(pattern_matches("10.0.0.0/8", "10.1.2.3", 80));
        assert!(!pattern_matches("10.0.0.0/8", "11.1.2.3", 80));
        assert!(pattern_matches("10.0.0.0/24", "10.0.0.55", 80));
        assert!(!pattern_matches("10.0.0.0/24", "10.0.1.55", 80));
        assert!(pattern_matches("0.0.0.0/0", "203.0.113.1", 80));
    }

    #[test]
    fn pattern_match_cidr_v6() {
        assert!(pattern_matches("2001:db8::/32", "2001:db8:cafe::1", 80));
        assert!(!pattern_matches("2001:db8::/32", "2001:db9::1", 80));
        assert!(pattern_matches("::/0", "2001:db8::1", 80));
    }

    #[test]
    fn pattern_match_ip_literal() {
        assert!(pattern_matches("127.0.0.1", "127.0.0.1", 80));
        assert!(!pattern_matches("127.0.0.1", "127.0.0.2", 80));
        assert!(pattern_matches("::1", "::1", 80));
    }

    #[test]
    fn parse_uri_handles_basic_shapes() {
        let u = parse_uri_min("http://example.com/path").unwrap();
        assert_eq!(u.scheme, "http");
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 80);

        let u = parse_uri_min("https://example.com:8443/").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.port, 8443);

        let u = parse_uri_min("http://user:pwd@host.example/").unwrap();
        assert_eq!(u.host, "host.example");

        let u = parse_uri_min("http://[2001:db8::1]:8080/").unwrap();
        assert_eq!(u.host, "2001:db8::1");
        assert_eq!(u.port, 8080);

        assert!(parse_uri_min("not a uri").is_none());
    }

    #[test]
    fn cidr_v4_basic_arithmetic() {
        assert!(v4_cidr(
            Ipv4Addr::new(10, 0, 0, 0),
            8,
            Ipv4Addr::new(10, 1, 2, 3)
        ));
        assert!(!v4_cidr(
            Ipv4Addr::new(10, 0, 0, 0),
            8,
            Ipv4Addr::new(11, 1, 2, 3)
        ));
        assert!(v4_cidr(
            Ipv4Addr::new(192, 168, 1, 0),
            24,
            Ipv4Addr::new(192, 168, 1, 5)
        ));
        assert!(!v4_cidr(
            Ipv4Addr::new(192, 168, 1, 0),
            24,
            Ipv4Addr::new(192, 168, 2, 5)
        ));
        assert!(v4_cidr(Ipv4Addr::UNSPECIFIED, 0, Ipv4Addr::new(1, 2, 3, 4)));
    }

    #[test]
    fn cidr_v6_basic_arithmetic() {
        let net: Ipv6Addr = "2001:db8::".parse().unwrap();
        let inside: Ipv6Addr = "2001:db8:cafe::1".parse().unwrap();
        let outside: Ipv6Addr = "2001:db9:cafe::1".parse().unwrap();
        assert!(v6_cidr(net, 32, inside));
        assert!(!v6_cidr(net, 32, outside));
        assert!(v6_cidr(Ipv6Addr::UNSPECIFIED, 0, inside));
    }

    #[test]
    fn record_failure_keeps_bounded_log() {
        // Drain whatever was there.
        failures().lock().clear();
        for i in 0..200 {
            record_failure(format!("http://h{i}/"), format!("h{i}"), 80);
        }
        let g = failures().lock();
        assert_eq!(g.len(), MAX_FAILURE_LOG);
        // First entry is from the *last* MAX_FAILURE_LOG events.
        assert!(g.first().unwrap().host.starts_with("h"));
    }

    #[test]
    fn anchor_strings_compiles() {
        _anchor_strings();
    }
}
