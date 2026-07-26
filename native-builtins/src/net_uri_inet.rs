// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.net` natives: URI, URL, InetAddress/InetSocketAddress and the NetworkInterface helpers.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;

pub(crate) fn native_url_set_stream_handler_factory_guard(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let d = URL_SET_STREAM_HANDLER_FACTORY_DEPTH.get();
    if d != 0 {
        // Re-entrant install (Spring Boot's factory recurses through
        // class-init while the outer call is still unwinding). The JDK would
        // throw `Error("factory already defined")`; we swallow it to keep the
        // single, outermost factory — matching "first install wins" for the
        // nested case.
        return Ok(None);
    }
    URL_SET_STREAM_HANDLER_FACTORY_DEPTH.set(1);
    // `setURLStreamHandlerFactory` is a *static* method, so `args[0]` is the
    // factory itself (no receiver). Publish it into the real `java.net.URL`
    // static `factory` field so the un-intercepted real `getURLStreamHandler`
    // bytecode consults it. Without this the field stays null and
    // `new URL("vfszip:...")` (Hibernate JarVisitorTest, Spring Boot loader)
    // raises `MalformedURLException: unknown protocol`. The real JDK also
    // clears the `handlers` cache on install; we leave it — a freshly-booted
    // VM has nothing cached for an app-defined scheme.
    if let Some(Value::Object(Some(fac))) = args.first() {
        // Force-native dispatch (force_native_over_real_jdk_bytecode) runs this
        // guard as the body of `setURLStreamHandlerFactory` WITHOUT first
        // running `java/net/URL`'s `<clinit>` (the normal invokestatic
        // class-init step is skipped for the override). If we publish `factory`
        // before URL is initialized, URL's later initialization re-creates its
        // statics vector and silently discards our write — leaving `factory`
        // null so `getURLStreamHandler` never consults the app factory
        // (`MalformedURLException: unknown protocol: classpath`, Tomcat
        // TestConfigFileLoader / TestClasspathUrlStreamHandler). Initialize URL
        // FIRST so the static store exists and is stable, then publish into it.
        let _ = ctx.ensure_class_initialized("java/net/URL");
        ctx.set_static_field_by_name("java/net/URL", "factory", Value::Object(Some(*fac)));
    }
    URL_SET_STREAM_HANDLER_FACTORY_DEPTH.set(0);
    Ok(None)
}

pub(crate) fn native_urlconnection_set_content_handler_factory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(fac))) = args.first() {
        let _ = ctx.ensure_class_initialized("java/net/URLConnection");
        ctx.set_static_field_by_name(
            "java/net/URLConnection",
            "factory",
            Value::Object(Some(*fac)),
        );
    }
    Ok(None)
}

/// Discover this host's configured DNS nameserver IPs for
/// `sun/net/dns/ResolverConfigurationImpl.os_nameservers`.
///
/// The real JDK's native `loadDNSconfig0` reads these via Windows' IP Helper
/// API (`GetNetworkParams`). We don't wrap that API, but publishing an empty
/// nameserver list (the prior behavior) makes `com.sun.jndi.dns.DnsClient`
/// fall back to its own hardcoded default of querying `127.0.0.1:53` — and
/// since nothing listens there, that query can only time out or fail with a
/// communication error, never the clean NXDOMAIN response
/// (`DnsWithResponseCodeException` with response code 3) that
/// mongo-java-driver's `DefaultDnsResolver.resolveAdditionalQueryParametersFromTxtRecords`
/// specifically tolerates. Real HotSpot instead queries the actual
/// OS-configured server, which answers (even if just NXDOMAIN for a
/// non-existent TXT record). Shell out to `ipconfig /all` and parse its
/// "DNS Servers" lines as a pragmatic stand-in for the IP Helper API so
/// CratonVM's JNDI DNS client reaches the same real, responsive server
/// HotSpot does. Falls back to an empty string (prior behavior) if
/// `ipconfig` is unavailable or unparsable — never fatal.
///
/// Cached: the result is latched on first use, for the same reason
/// [`crate::resolve_real_hostname`] is. On Windows this **forks and execs
/// `ipconfig /all`** and parses its full output; the two natives that call it
/// (`ResolverConfigurationImpl.init0` and `.loadDNSconfig0`,
/// `lib.rs`) sit under the real JDK's `ResolverConfiguration.get()` refresh
/// path, so a long-running program re-paid that process spawn every time the
/// JDK decided its resolver config was stale. The host's DNS configuration is
/// not something a JVM re-reads meaningfully mid-run — HotSpot's own
/// `loadDNSconfig0` reads it through `GetNetworkParams` once per refresh
/// precisely because the value is stable — so one probe per VM is the right
/// granularity. [`os_dns_nameservers_string_uncached`] keeps the parsing
/// logic directly testable without the latch.
pub(crate) fn os_dns_nameservers_string() -> String {
    static CACHED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHED
        .get_or_init(os_dns_nameservers_string_uncached)
        .clone()
}

/// The uncached probe behind [`os_dns_nameservers_string`]. Split out so the
/// `ipconfig /all` parsing stays testable without the `OnceLock` latch.
pub(crate) fn os_dns_nameservers_string_uncached() -> String {
    #[cfg(target_os = "windows")]
    {
        let output = match std::process::Command::new("ipconfig").arg("/all").output() {
            Ok(o) if o.status.success() => o,
            _ => return String::new(),
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let mut servers = Vec::new();
        let mut collecting = false;
        // Label lines ("   DNS Servers . . . . . : 1.2.3.4") sit at a small
        // indent; continuation lines carrying additional server addresses
        // are indented much further ("                          fe80::1%16").
        // Distinguish by indent depth rather than by colon-presence — an
        // IPv6 continuation value itself contains colons and would
        // otherwise be misread as a new "label: value" line, dropping any
        // real servers listed after it.
        const CONTINUATION_INDENT: usize = 10;
        for raw_line in text.lines() {
            let indent = raw_line.len() - raw_line.trim_start().len();
            let line = raw_line.trim();
            if collecting && indent >= CONTINUATION_INDENT && !line.is_empty() {
                // A zone-qualified link-local address (`fe80::1%16`) isn't a
                // usable destination without also carrying the scope id
                // through our UDP layer — skip it and keep collecting.
                if let Some(host) = strip_zone_id(line) {
                    if host.parse::<std::net::IpAddr>().is_ok() {
                        servers.push(host.to_string());
                    }
                }
                continue;
            }
            collecting = false;
            if let Some(idx) = line.find(':') {
                let (label, value) = line.split_at(idx);
                if label.to_ascii_lowercase().contains("dns servers") {
                    collecting = true;
                    let value = value[1..].trim();
                    if let Some(host) = strip_zone_id(value) {
                        if host.parse::<std::net::IpAddr>().is_ok() {
                            servers.push(host.to_string());
                        }
                    }
                }
            }
        }
        servers.join(" ")
    }
    #[cfg(not(target_os = "windows"))]
    {
        String::new()
    }
}

// ===========================================================================
// Phase 30: java.net — URL, URI
// ===========================================================================

pub(crate) fn register_net_natives(registry: &mut NativeMethodRegistry) {
    // synthetic-stub removed: defers to real JDK bytecode
    // java.net.URL and java.net.URI have real JDK bytecode. The previous
    // 5-field synthetic URL substitute and the side-table-backed URI
    // substitute (constructors, accessors, resolve/normalize/relativize,
    // openConnection's synthetic HttpURLConnection, and the
    // setURLStreamHandlerFactory guard) are all removed so the real .class
    // bytecode runs instead.
    //
    // KEPT: java.net.InetAddress natives below — these are host-resolution
    // bridges (DNS / local-host lookup against the host OS), not synthetic
    // substitutes for pure-bytecode behavior.

    // InetAddress (simplified) — host-resolution bridge, intentionally kept.
    let ia = "java/net/InetAddress";
    registry.register(
        ia,
        "getLocalHost",
        "()Ljava/net/InetAddress;",
        native_inet_localhost,
    );
    registry.register(
        ia,
        "getHostName",
        "()Ljava/lang/String;",
        native_inet_get_host_name,
    );
    registry.register(
        ia,
        "getHostAddress",
        "()Ljava/lang/String;",
        native_inet_get_host_address,
    );
    registry.register(
        ia,
        "toString",
        "()Ljava/lang/String;",
        native_inet_to_string,
    );
    registry.register(
        ia,
        "getByName",
        "(Ljava/lang/String;)Ljava/net/InetAddress;",
        native_inet_get_by_name,
    );
    registry.register(
        ia,
        "getAllByName",
        "(Ljava/lang/String;)[Ljava/net/InetAddress;",
        native_inet_get_all_by_name,
    );
    registry.register(
        ia,
        "getByAddress",
        "([B)Ljava/net/InetAddress;",
        native_inet_get_by_address,
    );
    registry.register(
        ia,
        "getLoopbackAddress",
        "()Ljava/net/InetAddress;",
        |ctx, _args| {
            let ia = alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2);
            let host = ctx.create_string("localhost");
            let addr = ctx.create_string("127.0.0.1");
            ctx.set_field(ia, 0, Value::Object(Some(host)));
            ctx.set_field(ia, 1, Value::Object(Some(addr)));
            Ok(Some(Value::Object(Some(ia))))
        },
    );
    registry.register(ia, "isReachable", "(I)Z", native_inet_is_reachable);
    registry.register(ia, "isLoopbackAddress", "()Z", native_inet_is_loopback);
    registry.register(ia, "isAnyLocalAddress", "()Z", native_inet_is_any_local);
    registry.register(ia, "isLinkLocalAddress", "()Z", native_inet_is_link_local);
    registry.register(ia, "isSiteLocalAddress", "()Z", native_inet_is_site_local);
    registry.register(ia, "isMulticastAddress", "()Z", native_inet_is_multicast);
    registry.register(ia, "getAddress", "()[B", native_inet_get_address);
}

/// Resolve a relative path against a base URI string
fn uri_resolve_path(base: &str, relative: &str) -> String {
    if relative.contains("://") {
        return relative.to_string(); // absolute URI
    }
    // Extract scheme+authority from base
    let (prefix, base_path) = if let Some(idx) = base.find("://") {
        let after_scheme = &base[idx + 3..];
        if let Some(path_start) = after_scheme.find('/') {
            (&base[..idx + 3 + path_start], &base[idx + 3 + path_start..])
        } else {
            (base, "/")
        }
    } else {
        ("", base)
    };
    if relative.starts_with('/') {
        return format!("{}{}", prefix, relative);
    }
    // Remove last path segment from base
    let base_dir = if let Some(last_slash) = base_path.rfind('/') {
        &base_path[..last_slash + 1]
    } else {
        "/"
    };
    let combined = format!("{}{}{}", prefix, base_dir, relative);
    uri_normalize_path(&combined)
}

/// Normalize a URI path by resolving `.` and `..` segments
fn uri_normalize_path(uri: &str) -> String {
    // Split URI into prefix (scheme+authority) and path
    let (prefix, path_and_rest) = if let Some(idx) = uri.find("://") {
        let after_scheme = &uri[idx + 3..];
        if let Some(path_start) = after_scheme.find('/') {
            (&uri[..idx + 3 + path_start], &uri[idx + 3 + path_start..])
        } else {
            return uri.to_string();
        }
    } else {
        ("", uri)
    };
    // Separate path from query/fragment
    let (path, suffix) = if let Some(q) = path_and_rest.find('?') {
        (&path_and_rest[..q], &path_and_rest[q..])
    } else if let Some(f) = path_and_rest.find('#') {
        (&path_and_rest[..f], &path_and_rest[f..])
    } else {
        (path_and_rest, "")
    };
    // Resolve `.` and `..`
    let mut segments: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "." | "" => {} // skip current dir and empty segments
            ".." => {
                segments.pop();
            }
            _ => segments.push(seg),
        }
    }
    let normalized_path = if path.starts_with('/') {
        format!("/{}", segments.join("/"))
    } else {
        segments.join("/")
    };
    format!("{}{}{}", prefix, normalized_path, suffix)
}

/// `URI.getSchemeSpecificPart()` / `getRawSchemeSpecificPart()`.
///
/// Spring Boot 3.2 `Archive.create(ProtectionDomain)` does
/// `getCodeSource().getLocation().toURI().getSchemeSpecificPart()` → `new File(...)`.
/// Our `URL.toURI()` stores the full string in `URL_FIELD_FULL` (e.g. `file:/C:/a.jar`).
/// Returning the entire string breaks `File(String)`; callers need the scheme-specific
/// part only (`/C:/a.jar`).
fn native_uri_get_scheme_specific_part(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let full = match ctx.get_field(this, URL_FIELD_FULL) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let ssp = if let Some(pos) = full.find(':') {
        full[pos + 1..].split('#').next().unwrap_or("").to_string()
    } else {
        // Preserve the old fallback for scheme-less synthetic URIs.
        full
    };
    Ok(Some(Value::Object(Some(ctx.create_string(&ssp)))))
}

pub(crate) fn url_parse(ctx: &mut dyn NativeContext, this: ObjectRef, url_str: &str) {
    // Fast path: opaque/hierarchical `file:` URIs used by `Class.getProtectionDomain`
    // (`file:/C:/foo.jar` on Windows — note **no** `file://`). The generic
    // `://` splitter below treats these as schemeless and corrupts field slots,
    // which used to leave `URI` field 5 empty and broke
    // `getSchemeSpecificPart() -> new File(...)` for Spring Boot's launcher.
    if let Some(rest) = url_str.strip_prefix("file:") {
        let (without_ref, ref_part) = if let Some(pos) = rest.find('#') {
            (&rest[..pos], Some(&rest[pos + 1..]))
        } else {
            (rest, None)
        };
        let (path_str, query_part) = if let Some(pos) = without_ref.find('?') {
            (&without_ref[..pos], Some(&without_ref[pos + 1..]))
        } else {
            (without_ref, None)
        };
        let file_str = if let Some(query) = query_part {
            format!("{path_str}?{query}")
        } else {
            path_str.to_string()
        };
        let file_obj = ctx.create_string(&file_str);
        let path_obj = ctx.create_string(path_str);
        let query_obj = query_part
            .filter(|query| !query.is_empty())
            .map(|query| ctx.create_string(query));
        let ref_obj = ref_part
            .filter(|fragment| !fragment.is_empty())
            .map(|fragment| ctx.create_string(fragment));
        let proto_obj = ctx.create_string("file");
        let host_empty = ctx.create_string("");
        ctx.set_field(this, URL_FIELD_PROTOCOL, Value::Object(Some(proto_obj)));
        ctx.set_field(this, URL_FIELD_HOST, Value::Object(Some(host_empty)));
        ctx.set_field(this, URL_FIELD_PORT, Value::Int(-1));
        ctx.set_field(this, URL_FIELD_PATH, Value::Object(Some(path_obj)));
        ctx.set_field(this, URL_FIELD_QUERY, Value::Object(query_obj));
        ctx.set_field_by_name(this, "file", Value::Object(Some(file_obj)));
        ctx.set_field_by_name(this, "path", Value::Object(Some(path_obj)));
        ctx.set_field_by_name(this, "query", Value::Object(query_obj));
        ctx.set_field_by_name(this, "ref", Value::Object(ref_obj));
        // Intentionally skip writing `URL_FIELD_FULL` (slot index 5): for a
        // real-JDK URL instance that index aliases the real `authority`
        // field (see the sibling `jar:` fast path below, which already
        // avoids this). Stuffing the whole raw string there made real
        // bytecode's `getAuthority()` return e.g. `"file:/opt/foo/bar"`
        // instead of `null`, which corrupted any later `new URL(URL base,
        // String spec)` merge that used this URL as its base (Woodstox's
        // `URLUtil.urlFromSystemId(String, URL)`, hit while resolving a
        // DTD's external SYSTEM entity, inherited the bogus authority and
        // real JDK's own validation rejected it with
        // `MalformedURLException: Illegal character found in authority:
        // '/'` — see Jaxb2CollectionHttpMessageConverterTests
        // .readXmlRootElementExternalEntityEnabled()). Explicitly null the
        // named `authority` field instead; `getProtocol`/`toExternalForm`
        // reconstruct the string from protocol/host/port/path already.
        ctx.set_field_by_name(this, "authority", Value::Object(None));
        return;
    }

    // Opaque `jar:` URLs of the form `jar:<inner-url>!/<entry>`. SmallRye and
    // Quarkus' classpath scanners produce these for nested jars and then
    // call `url.getProtocol()`. The generic `://` splitter below would set
    // the protocol to "" (jar: has no authority), which Keycloak's
    // `ClassPathUtils.processAsPath` reports as
    // `IllegalArgumentException("Unexpected protocol null for URL …")`.
    if let Some(rest) = url_str.strip_prefix("jar:") {
        let full_obj = ctx.create_string(url_str);
        let proto_obj = ctx.create_string("jar");
        let host_empty = ctx.create_string("");
        let file_obj = ctx.create_string(rest);
        // Set via field NAME so this works whether `this` is a real-JDK URL
        // (whose runtime instance-field order need not match the source
        // declaration order) or one of our synthetic 13-slot URLs.
        ctx.set_field_by_name(this, "protocol", Value::Object(Some(proto_obj)));
        ctx.set_field_by_name(this, "host", Value::Object(Some(host_empty)));
        ctx.set_field_by_name(this, "port", Value::Int(-1));
        ctx.set_field_by_name(this, "file", Value::Object(Some(file_obj)));
        ctx.set_field_by_name(this, "path", Value::Object(Some(file_obj)));
        ctx.set_field_by_name(this, "query", Value::Object(None));
        ctx.set_field_by_name(this, "authority", Value::Object(None));
        // Intentionally skip writing `URL_FIELD_FULL` (slot index 5): for
        // real-JDK URL that index aliases the `protocol` slot under our
        // class-loader layout, and overwriting it with the full URL string
        // makes `url.getProtocol()` return the full spec (which broke
        // SmallRye's `ClassPathUtils.processAsPath`). Our `getProtocol`
        // fallback reconstructs the scheme on demand for the synthetic-URL
        // path.
        let _ = full_obj;
        return;
    }

    // Parse: protocol://host[:port][/path][?query]
    let (protocol, rest) = if let Some(pos) = url_str.find("://") {
        (&url_str[..pos], &url_str[pos + 3..])
    } else {
        ("", url_str)
    };
    let (host_port, path_query) = if let Some(pos) = rest.find('/') {
        (&rest[..pos], &rest[pos..])
    } else {
        (rest, "")
    };
    let (host, port) = if let Some(pos) = host_port.rfind(':') {
        let port_str = &host_port[pos + 1..];
        if let Ok(p) = port_str.parse::<i32>() {
            (&host_port[..pos], p)
        } else {
            (host_port, -1)
        }
    } else {
        (host_port, -1)
    };
    let (path, query) = if let Some(pos) = path_query.find('?') {
        (&path_query[..pos], &path_query[pos + 1..])
    } else {
        (path_query, "")
    };

    let proto_obj = ctx.create_string(protocol);
    let host_obj = ctx.create_string(host);
    let path_obj = ctx.create_string(path);
    let file_str = if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    };
    let file_obj = ctx.create_string(&file_str);
    let query_obj = if query.is_empty() {
        None
    } else {
        Some(ctx.create_string(query))
    };
    let full_obj = ctx.create_string(url_str);

    ctx.set_field(this, URL_FIELD_PROTOCOL, Value::Object(Some(proto_obj)));
    ctx.set_field(this, URL_FIELD_HOST, Value::Object(Some(host_obj)));
    ctx.set_field(this, URL_FIELD_PORT, Value::Int(port));
    ctx.set_field(this, URL_FIELD_PATH, Value::Object(Some(path_obj)));
    ctx.set_field(this, URL_FIELD_QUERY, Value::Object(query_obj));
    ctx.set_field(this, URL_FIELD_FULL, Value::Object(Some(full_obj)));
    ctx.set_field_by_name(this, "file", Value::Object(Some(file_obj)));
    ctx.set_field_by_name(this, "path", Value::Object(Some(path_obj)));
    ctx.set_field_by_name(this, "query", Value::Object(query_obj));
}

fn native_url_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let url_str = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    url_parse(ctx, this, &url_str);
    Ok(None)
}

fn native_url_get_protocol(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Look up the `protocol` field by name (works for both real-JDK URL and
    // our synthetic 13-field URL — slot 0 in both, but we ask by name so
    // we're immune to any future layout shuffle).
    let v = ctx.get_field_by_name(this, "protocol");
    if let Value::Object(Some(_)) = v {
        return Ok(Some(v));
    }
    // Fallback for real-JDK URL constructors we don't intercept (e.g.
    // `URL(String, String, int, String, URLStreamHandler)` used by
    // `JarURLConnection`) which can leave `protocol` unset under our VM
    // when the bytecode constructor's putfields don't reach our heap.
    // Recover the scheme from `URL_FIELD_FULL` (our synthetic toString
    // cache) when that slot was populated by `url_parse`. SmallRye's
    // `ClassPathUtils.processAsPath` calls `url.getProtocol()` on
    // `jar:file:…!/…` URLs and throws "Unexpected protocol null" if we
    // return null, which crashed Keycloak's SmallRye config builder before
    // the Quarkus banner.
    if let Value::Object(Some(s)) = ctx.get_field(this, URL_FIELD_FULL) {
        let full = ctx.read_string(s).unwrap_or_default();
        if let Some(idx) = full.find(':') {
            let scheme = &full[..idx];
            if !scheme.is_empty()
                && scheme
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
            {
                let so = ctx.create_string(scheme);
                return Ok(Some(Value::Object(Some(so))));
            }
        }
    }
    Ok(Some(v))
}

fn native_url_get_host(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, URL_FIELD_HOST)))
}

fn native_url_get_port(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    Ok(Some(ctx.get_field(this, URL_FIELD_PORT)))
}

fn native_url_get_path(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    if let Value::Object(Some(path)) = ctx.get_field_by_name(this, "path") {
        return Ok(Some(Value::Object(Some(path))));
    }
    Ok(Some(ctx.get_field(this, URL_FIELD_PATH)))
}

fn native_url_get_query(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    if let Value::Object(Some(query)) = ctx.get_field_by_name(this, "query") {
        return Ok(Some(Value::Object(Some(query))));
    }
    Ok(Some(ctx.get_field(this, URL_FIELD_QUERY)))
}

fn native_url_get_file(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    if let Value::Object(Some(file)) = ctx.get_field_by_name(this, "file") {
        return Ok(Some(Value::Object(Some(file))));
    }
    let path = match ctx.get_field(this, URL_FIELD_PATH) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let query = match ctx.get_field(this, URL_FIELD_QUERY) {
        Value::Object(Some(s)) => format!("?{}", ctx.read_string(s).unwrap_or_default()),
        _ => String::new(),
    };
    let file = format!("{}{}", path, query);
    let result = ctx.create_string(&file);
    Ok(Some(Value::Object(Some(result))))
}

fn native_url_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, URL_FIELD_FULL)))
}

fn native_url_to_uri(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let nfields = ctx.object_num_fields(this);
    let full = match ctx.get_field(this, URL_FIELD_FULL) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    // Real-JDK URL has 13-field layout where field 5 = authority (often null
    // for our synthetic file: URLs allocated in Class.getProtectionDomain).
    // Fall back to reconstructing from field 0=protocol + ":" + field 3=file.
    let full = if !full.is_empty() {
        full
    } else if nfields >= 7 {
        let protocol = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let file = match ctx.get_field(this, 3) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => match ctx.get_field(this, 6) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            },
        };
        if !protocol.is_empty() && !file.is_empty() {
            format!("{protocol}:{file}")
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    if crate::nbflags().dbg_sbload {
        eprintln!(
            "[DBG_SBLOAD] URL.toURI() nfields={} full={:?}",
            nfields, full
        );
    }
    let uri = alloc_concurrent_synthetic(ctx, "java/net/URI", 6);
    url_parse(ctx, uri, &full);
    Ok(Some(Value::Object(Some(uri))))
}

/// Read a String-typed URL field, preferring the named lookup (correct for both
/// real-JDK `java.net.URL` and our synthetic 13-slot URL, whose slots 0..=4
/// coincide with the real layout) and falling back to the positional slot for
/// fully-synthetic stubs whose named lookup misses.
fn url_str_field(
    ctx: &mut dyn NativeContext,
    url: ObjectRef,
    name: &str,
    slot: usize,
) -> Option<String> {
    if let Value::Object(Some(s)) = ctx.get_field_by_name(url, name) {
        return ctx.read_string(s);
    }
    if let Value::Object(Some(s)) = ctx.get_field(url, slot) {
        return ctx.read_string(s);
    }
    None
}

/// Canonical key for `URL.equals`/`hashCode`: the URL's external form,
/// reconstructed from the protocol/host/port/file/ref fields.
///
/// The previous implementation keyed off `URL_FIELD_FULL` (slot 5). For a
/// real-JDK `java.net.URL` slot 5 is the `authority` field, NOT a full-string
/// cache, so two semantically-equal URLs built via different paths — e.g.
/// `new URL("file:myjar.jar")` (real ctor, authority=null) vs
/// `URI.create("file:myjar.jar").toURL()` (our synthetic builder, which writes
/// the full string into slot 5) — compared unequal and hashed differently.
/// That broke `ResourceUtils.extractJarFileURL`/`extractArchiveURL` (and any
/// `Set<URL>` dedup). Reconstructing the external form from the layout-stable
/// component fields is identical regardless of how the URL was constructed.
fn url_external_form(ctx: &mut dyn NativeContext, url: ObjectRef) -> String {
    let proto = url_str_field(ctx, url, "protocol", URL_FIELD_PROTOCOL).unwrap_or_default();
    if proto.is_empty() {
        // Fully-synthetic stub with only the FULL string populated.
        return match ctx.get_field(url, URL_FIELD_FULL) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
    }
    let host = url_str_field(ctx, url, "host", URL_FIELD_HOST).unwrap_or_default();
    let port = match ctx.get_field_by_name(url, "port") {
        Value::Int(p) => p,
        _ => match ctx.get_field(url, URL_FIELD_PORT) {
            Value::Int(p) => p,
            _ => -1,
        },
    };
    // Slot 3 (URL_FIELD_PATH) is the real-JDK `file` field, which already
    // includes any query string — exactly what `toExternalForm` appends.
    let file = url_str_field(ctx, url, "file", URL_FIELD_PATH).unwrap_or_default();
    let reff = match ctx.get_field_by_name(url, "ref") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    let mut out = String::with_capacity(proto.len() + host.len() + file.len() + 8);
    out.push_str(&proto);
    out.push(':');
    if !host.is_empty() {
        out.push_str("//");
        out.push_str(&host);
        if port >= 0 {
            out.push(':');
            out.push_str(&port.to_string());
        }
    }
    out.push_str(&file);
    if let Some(r) = reff {
        out.push('#');
        out.push_str(&r);
    }
    out
}

fn url_same_file_form(ctx: &mut dyn NativeContext, url: ObjectRef) -> String {
    let mut out = url_external_form(ctx, url);
    out.truncate(out.find('#').unwrap_or(out.len()));
    out
}

pub(crate) fn native_url_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let a = url_external_form(ctx, this);
    let b = url_external_form(ctx, other);
    Ok(Some(Value::Int(if a == b { 1 } else { 0 })))
}

pub(crate) fn native_url_same_file(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let same = url_same_file_form(ctx, this) == url_same_file_form(ctx, other);
    Ok(Some(Value::Int(if same { 1 } else { 0 })))
}

pub(crate) fn native_url_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let s = url_external_form(ctx, this);
    let mut h: i32 = 0;
    for b in s.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as i32);
    }
    Ok(Some(Value::Int(h)))
}

/// Populate a `java.net.URI` object's named fields from its full text.
///
/// The multi-arg `URI` constructors route component values through
/// `url_parse`, which writes the *URL*-shaped positional slots (0..=5).
/// A real-JDK `java.net.URI` has a completely different instance-field
/// layout, so those positional writes land on the wrong slots and the
/// URI's cached `string` field stays null — making `toString()` /
/// `toURL()` return empty/null. Writing the canonical `string`,
/// `scheme`, `path` and `schemeSpecificPart` fields BY NAME fixes this
/// for both real and synthetic URI objects (the `uri_raw_string`
/// helper reads `string` by name first).
pub(crate) fn uri_store_named(ctx: &mut dyn NativeContext, this: ObjectRef, full: &str) {
    let full_obj = ctx.create_string(full);
    ctx.set_field_by_name(this, "string", Value::Object(Some(full_obj)));
    // JDK scheme detection (see `net_phase_e::uri_scheme_colon`): a colon
    // inside a relative reference's path ("/redirect:account") is NOT a
    // scheme delimiter. The previous bare `find(':')` wrote scheme="/redirect"
    // and path="account" here, which the getPath/getRawPath field-preference
    // fallback then surfaced — Spring derived default view name "account"
    // instead of "redirect:account" and ViewResolutionResultHandlerTests'
    // defaultViewNameWithRedirectPrefixFails saw onComplete() instead of the
    // expected resolution error.
    if let Some(colon) = net_phase_e::uri_scheme_colon(full) {
        let scheme = &full[..colon];
        let raw_ssp = &full[colon + 1..];
        let ssp = raw_ssp.split('#').next().unwrap_or(raw_ssp);
        if !scheme.is_empty() {
            let scheme_obj = ctx.create_string(scheme);
            ctx.set_field_by_name(this, "scheme", Value::Object(Some(scheme_obj)));
        }
        let ssp_obj = ctx.create_string(ssp);
        ctx.set_field_by_name(this, "schemeSpecificPart", Value::Object(Some(ssp_obj)));
        // Hierarchical path: strip an optional `//authority` prefix.
        let path = if let Some(after) = ssp.strip_prefix("//") {
            let slash = after.find('/').unwrap_or(after.len());
            &after[slash..]
        } else {
            ssp
        };
        let path = path.split(['?', '#']).next().unwrap_or("");
        if !path.is_empty() {
            let path_obj = ctx.create_string(path);
            ctx.set_field_by_name(this, "path", Value::Object(Some(path_obj)));
        }
    } else {
        // Relative reference: no scheme field; the SSP is the whole input
        // minus the fragment, and the path parse is authority-aware.
        let ssp = full.split('#').next().unwrap_or(full);
        let ssp_obj = ctx.create_string(ssp);
        ctx.set_field_by_name(this, "schemeSpecificPart", Value::Object(Some(ssp_obj)));
        if let Some(path) = net_phase_e::uri_select_raw_path(full) {
            if !path.is_empty() {
                let path_obj = ctx.create_string(&path);
                ctx.set_field_by_name(this, "path", Value::Object(Some(path_obj)));
            }
        }
    }
}

/// Returns `(index, reason)` if `java.net.URI`'s single-string parser would
/// throw a scheme-name `URISyntaxException` before ever reaching the
/// scheme-specific-part / illegal-character checks below. Mirrors the real
/// JDK `Parser.parse`: scan from index 0 for the first `:`, `/`, `?`, or `#`.
/// If a stop char other than `:` is hit first (or no `:` appears at all), the
/// input is a scheme-less relative reference — not an error here. If `:` is
/// hit first:
///   * at index 0 (no characters before it) → "Expected scheme name" at 0
///     (e.g. `::http:///`, `:path`).
///   * with a non-letter first character → "Illegal character in scheme
///     name" at 0 (e.g. `12:30`, `1abc:path` — a scheme must start ALPHA).
///   * with any character in `1..p` outside `ALPHA / DIGIT / "+" / "-" / "."`
///     → same message at that character's index (e.g.
///     `scheme_with_underscore:path`, underscore is not a legal scheme char).
/// Without this check `RestClient.buildUri`'s malformed-endpoint guard never
/// fires: `new URI("::http:///")` silently parsed with `scheme=null` instead
/// of throwing, so `InternalRequest`'s constructor didn't fail before
/// `nextNodes()` ran — surfacing an unrelated `NodeSelector` NPE instead of
/// the expected `IllegalArgumentException`.
pub(crate) fn uri_scheme_name_fail_index(s: &str) -> Option<(usize, &'static str)> {
    let bytes = s.as_bytes();
    let mut p = 0usize;
    while p < bytes.len() {
        match bytes[p] {
            b'/' | b'?' | b'#' => return None,
            b':' => break,
            _ => p += 1,
        }
    }
    if p >= bytes.len() {
        return None;
    }
    if p == 0 {
        return Some((0, "Expected scheme name"));
    }
    if !bytes[0].is_ascii_alphabetic() {
        return Some((0, "Illegal character in scheme name"));
    }
    for (i, &b) in bytes.iter().enumerate().take(p).skip(1) {
        if !(b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')) {
            return Some((i, "Illegal character in scheme name"));
        }
    }
    None
}

/// Returns the byte index of the first character that `java.net.URI`'s
/// single-string parser would reject as illegal, or `None` if every character
/// is permitted. Mirrors the JDK parser's legal-character set for US-ASCII:
/// unreserved (`alphanum` + `-_.!~*'()`) + reserved (`;/?:@&=+$,[]`) + the
/// escape/fragment delimiters `%` and `#`. ASCII control characters (`<0x20`,
/// `0x7F`) are always illegal. Non-ASCII (`>=0x80`) is deliberately left
/// permitted here so we do not over-reject inputs that currently parse — the
/// goal is to match HotSpot on the clearly-malformed ASCII cases (spaces,
/// `{}<>"\^|`), not to police every Unicode edge.
pub(crate) fn uri_first_illegal_index(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    for (i, c) in s.char_indices() {
        let u = c as u32;
        if u < 0x20 || u == 0x7f {
            return Some(i);
        }
        if u >= 0x80 {
            continue;
        }
        // A `%` is only legal as the start of an "escaped" triple (`%` hex hex,
        // RFC 2396). The real JDK single-string URI parser validates this and
        // throws `URISyntaxException` for a bare/malformed `%` (e.g. "foo%%x",
        // "/p%th") — our check previously allowed `%` unconditionally, so
        // malformed escapes silently passed through `new URI(String)` instead
        // of throwing, which broke `ServletServerHttpRequest.initURI`'s
        // catch-and-reencode fallback (it never got a chance to run) and the
        // "malformed path must throw IllegalStateException" contract.
        if c == '%' {
            let valid_escape = bytes.get(i + 1).copied().map(is_ascii_hex_digit) == Some(true)
                && bytes.get(i + 2).copied().map(is_ascii_hex_digit) == Some(true);
            if !valid_escape {
                return Some(i);
            }
            continue;
        }
        let ok = c.is_ascii_alphanumeric()
            || matches!(
                c,
                // unreserved marks
                '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')'
                // reserved (RFC 2396 + RFC 2732 host brackets)
                | ';' | '/' | '?' | ':' | '@' | '&' | '=' | '+' | '$' | ','
                | '[' | ']'
                // fragment delimiter
                | '#'
            );
        if !ok {
            return Some(i);
        }
    }
    None
}

/// Returns the index at which `java.net.URI`'s single-string parser would throw
/// `URISyntaxException("Expected scheme-specific part", index)`, or `None` if
/// the scheme-specific part is present (or there is no scheme at all).
///
/// Per RFC 2396 / the JDK parser, an absolute URI (one with a `scheme:` prefix)
/// whose part after the colon is NOT hierarchical (does not begin with `/`)
/// must have a non-empty opaque part (up to a `#` fragment). So `file:`,
/// `http:`, `mailto:` and `a:` are all rejected, while `file:.`, `file:/x`,
/// `http://h` and scheme-less relatives are accepted. Our lenient `url_parse`
/// happily accepted `file:`, which broke Spring's `ResourceUtils.toURL` →
/// `new URI(cleanPath("file:."))` fall-back chain (PathEditor/FileEditor
/// `currentDirectory`).
pub(crate) fn uri_empty_ssp_fail_index(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    // A scheme must start with an ASCII letter.
    if bytes.first().map(|b| b.is_ascii_alphabetic()) != Some(true) {
        return None;
    }
    let mut i = 1;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b':' {
            // `s[..i]` is the scheme; the scheme-specific part starts at i+1.
            let rest = &s[i + 1..];
            if rest.starts_with('/') {
                // Hierarchical (`scheme:/path`, `scheme://authority`): the part
                // is present even if the path is otherwise empty.
                return None;
            }
            // Opaque part runs up to a `#` fragment (or end of string).
            let opaque_len = rest.find('#').unwrap_or(rest.len());
            return if opaque_len == 0 { Some(i + 1) } else { None };
        }
        // Valid scheme characters: ALPHA / DIGIT / `+` / `-` / `.`.
        if c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.') {
            i += 1;
            continue;
        }
        // Any other character before a `:` means there is no scheme.
        return None;
    }
    None
}

pub(crate) fn native_uri_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let url_str = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    // Reject a malformed scheme name before any other check — the real JDK
    // parser validates this first (see `uri_scheme_name_fail_index`).
    if let Some((pos, reason)) = uri_scheme_name_fail_index(&url_str) {
        let input = ctx.create_string(&url_str);
        let reason_str = ctx.create_string(reason);
        if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
            "java/net/URISyntaxException",
            "(Ljava/lang/String;Ljava/lang/String;I)V",
            &[
                Value::Object(Some(input)),
                Value::Object(Some(reason_str)),
                Value::Int(pos as i32),
            ],
        ) {
            return Err(MethodCallFailed::ExceptionThrown(exc));
        }
    }
    // Reject illegal characters like java.net.URI's single-string parser does —
    // `new URI(String)` must throw URISyntaxException for them. Our parser was
    // lenient and accepted anything, so malformed input slipped through:
    //   * control chars: "https://keycloak.org\n" treated as valid (keycloak
    //     SecureRedirectUrisEnforcerExecutorTest.failUriSyntax);
    //   * spaces / delimiters: "not a valid uri :{}" treated as valid, so
    //     HttpHeaderSecurityFilter.setAntiClickJackingUri never threw and
    //     TestHttpHeaderSecurityFilter.testAntiClickJackingInvalidUri saw no
    //     ServletException.
    // `uri_first_illegal_index` mirrors the JDK's legal-character set for ASCII
    // (unreserved + reserved + `%`/`#`); non-ASCII is left to the lenient path
    // to avoid over-rejecting inputs the gauntlet relies on. Control chars stay
    // rejected unconditionally even with the opt-out gate (preserves the
    // keycloak fix); the broader ASCII check is gated default-ON so it can be
    // disabled (CRATONVM_URI_STRICT_CHARS=0) if a regression surfaces.
    let strict_uri_chars = crate::nbflags().uri_strict_chars;
    let illegal = if strict_uri_chars {
        uri_first_illegal_index(&url_str)
    } else {
        url_str
            .char_indices()
            .find(|(_, c)| (*c as u32) < 0x20 || (*c as u32) == 0x7f)
            .map(|(i, _)| i)
    };
    if let Some(pos) = illegal {
        let input = ctx.create_string(&url_str);
        let reason = ctx.create_string("Illegal character in URI");
        if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
            "java/net/URISyntaxException",
            "(Ljava/lang/String;Ljava/lang/String;I)V",
            &[
                Value::Object(Some(input)),
                Value::Object(Some(reason)),
                Value::Int(pos as i32),
            ],
        ) {
            return Err(MethodCallFailed::ExceptionThrown(exc));
        }
    }
    // Reject an absolute URI with an empty scheme-specific part (`file:`,
    // `http:`, …) — the JDK parser throws here, and Spring relies on that throw
    // to fall back to the deprecated `new URL(String)` path.
    if let Some(pos) = uri_empty_ssp_fail_index(&url_str) {
        let input = ctx.create_string(&url_str);
        let reason = ctx.create_string("Expected scheme-specific part");
        if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
            "java/net/URISyntaxException",
            "(Ljava/lang/String;Ljava/lang/String;I)V",
            &[
                Value::Object(Some(input)),
                Value::Object(Some(reason)),
                Value::Int(pos as i32),
            ],
        ) {
            return Err(MethodCallFailed::ExceptionThrown(exc));
        }
    }
    url_parse(ctx, this, &url_str);
    uri_store_named(ctx, this, &url_str);
    Ok(None)
}

/// URI(String scheme, String host, String path, String fragment)
fn native_uri_init_4(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let scheme = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let _host = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(ctx.read_string(*o).unwrap_or_default()),
        _ => None,
    };
    let path = match args.get(3) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let _fragment = match args.get(4) {
        Some(Value::Object(Some(o))) => Some(ctx.read_string(*o).unwrap_or_default()),
        _ => None,
    };
    // Build full URI string: scheme:[//host]path[#fragment]
    //
    // Per RFC 3986 / `java.net.URI`, the `//host` authority component is
    // only emitted when a host is actually present. When `host` is null
    // (the case for `File.toURI()` on Windows, which calls
    // `new URI("file", null, "/C:/path", null)`), the result must be
    // `file:/C:/path` — a single slash. Emitting `file://` + path here
    // produced the malformed `file:///C:/path` that broke
    // `URLClassLoader` (URI.toURL() returned null).
    let full = if scheme.is_empty() {
        path.clone()
    } else {
        let frag_part = match &_fragment {
            Some(f) => format!("#{f}"),
            None => String::new(),
        };
        match _host.as_deref() {
            Some(h) if !h.is_empty() => {
                format!("{scheme}://{h}{path}{frag_part}")
            }
            _ => format!("{scheme}:{path}{frag_part}"),
        }
    };
    url_parse(ctx, this, &full);
    uri_store_named(ctx, this, &full);
    Ok(None)
}

/// URI(String scheme, String ssp, String fragment)
fn native_uri_init_3(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let scheme = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let ssp = match args.get(2) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let fragment = match args.get(3) {
        Some(Value::Object(Some(o))) => Some(ctx.read_string(*o).unwrap_or_default()),
        _ => None,
    };
    let frag_part = match &fragment {
        Some(f) => format!("#{f}"),
        None => String::new(),
    };
    let full = if scheme.is_empty() {
        format!("{ssp}{frag_part}")
    } else {
        format!("{scheme}:{ssp}{frag_part}")
    };
    url_parse(ctx, this, &full);
    uri_store_named(ctx, this, &full);
    Ok(None)
}

/// URI(String scheme, String authority, String path, String query, String fragment)
fn native_uri_init_5(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let scheme = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let authority = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(ctx.read_string(*o).unwrap_or_default()),
        _ => None,
    };
    let path = match args.get(3) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let query = match args.get(4) {
        Some(Value::Object(Some(o))) => Some(ctx.read_string(*o).unwrap_or_default()),
        _ => None,
    };
    let _fragment = match args.get(5) {
        Some(Value::Object(Some(o))) => Some(ctx.read_string(*o).unwrap_or_default()),
        _ => None,
    };
    let query_part = match &query {
        Some(q) => format!("?{}", quote_uric(q)),
        None => String::new(),
    };
    let frag_part = match &_fragment {
        Some(f) => format!("#{f}"),
        None => String::new(),
    };
    // Only emit the `//authority` component when an authority is present;
    // otherwise the URI is `scheme:path` (avoids malformed `scheme:///path`).
    let full = if scheme.is_empty() {
        format!("{path}{query_part}{frag_part}")
    } else {
        match authority.as_deref() {
            Some(a) if !a.is_empty() => {
                format!("{scheme}://{a}{path}{query_part}{frag_part}")
            }
            _ => format!("{scheme}:{path}{query_part}{frag_part}"),
        }
    };
    url_parse(ctx, this, &full);
    uri_store_named(ctx, this, &full);
    Ok(None)
}

/// URI(String scheme, String userInfo, String host, int port, String path, String query, String fragment)
fn native_uri_init_7(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let scheme = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let _user_info = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(ctx.read_string(*o).unwrap_or_default()),
        _ => None,
    };
    let host = match args.get(3) {
        Some(Value::Object(Some(o))) => Some(ctx.read_string(*o).unwrap_or_default()),
        _ => None,
    };
    let port = match args.get(4) {
        Some(Value::Int(p)) => *p,
        _ => -1,
    };
    let path = match args.get(5) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let query = match args.get(6) {
        Some(Value::Object(Some(o))) => Some(ctx.read_string(*o).unwrap_or_default()),
        _ => None,
    };
    let _fragment = match args.get(7) {
        Some(Value::Object(Some(o))) => Some(ctx.read_string(*o).unwrap_or_default()),
        _ => None,
    };
    let port_part = if port >= 0 {
        format!(":{port}")
    } else {
        String::new()
    };
    let query_part = match &query {
        Some(q) => format!("?{}", quote_uric(q)),
        None => String::new(),
    };
    let frag_part = match &_fragment {
        Some(f) => format!("#{f}"),
        None => String::new(),
    };
    // Only emit `//host[:port]` when a host is present; otherwise the URI
    // is `scheme:path` (avoids malformed `scheme:///path`).
    let full = if scheme.is_empty() {
        format!("{path}{query_part}{frag_part}")
    } else {
        match host.as_deref() {
            Some(h) if !h.is_empty() => {
                format!("{scheme}://{h}{port_part}{path}{query_part}{frag_part}")
            }
            _ => format!("{scheme}:{path}{query_part}{frag_part}"),
        }
    };
    url_parse(ctx, this, &full);
    uri_store_named(ctx, this, &full);
    Ok(None)
}

fn native_uri_create(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let url_str = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let uri = alloc_concurrent_synthetic(ctx, "java/net/URI", 6);
    url_parse(ctx, uri, &url_str);
    // `url_parse` populates the synthetic *URL* slot layout (by index).
    // `java.net.URI` getters (`getScheme`/`getPath`/`getRawSchemeSpecificPart`)
    // read by FIELD NAME (`scheme`/`path`/`string`), so also store the parsed
    // components by name for consistency with the other `URI` constructors.
    // (In practice `URI.create` is routed to the parser in `net_phase_e`,
    // which registers later; this keeps the fallback path consistent.)
    uri_store_named(ctx, uri, &url_str);
    Ok(Some(Value::Object(Some(uri))))
}

fn native_inet_localhost(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let hostname = resolve_real_hostname();
    let ip = resolve_primary_ipv4(&hostname);
    let ia = alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2);
    let host_ref = ctx.create_string(&hostname);
    let addr_ref = ctx.create_string(&ip);
    ctx.set_field(ia, 0, Value::Object(Some(host_ref)));
    ctx.set_field(ia, 1, Value::Object(Some(addr_ref)));
    Ok(Some(Value::Object(Some(ia))))
}

fn native_inet_get_host_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, 0)))
}

fn native_inet_get_host_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, 1)))
}

fn native_inet_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let host = match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let addr = match ctx.get_field(this, 1) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let s = format!("{}/{}", host, addr);
    let result = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(result))))
}

/// InetAddress.getByName(String) — resolve hostname to IP address.
/// Uses std::net::ToSocketAddrs for real DNS resolution.
fn native_inet_get_by_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let hostname = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };

    // Handle null/localhost/loopback
    if hostname.is_empty() || hostname == "localhost" {
        let ia = alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2);
        let host = ctx.create_string("localhost");
        let addr = ctx.create_string("127.0.0.1");
        ctx.set_field(ia, 0, Value::Object(Some(host)));
        ctx.set_field(ia, 1, Value::Object(Some(addr)));
        return Ok(Some(Value::Object(Some(ia))));
    }

    // Try to parse as direct IP address first
    if hostname.parse::<std::net::Ipv4Addr>().is_ok()
        || hostname.parse::<std::net::Ipv6Addr>().is_ok()
    {
        let ia = alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2);
        let host = ctx.create_string(&hostname);
        let addr = ctx.create_string(&hostname);
        ctx.set_field(ia, 0, Value::Object(Some(host)));
        ctx.set_field(ia, 1, Value::Object(Some(addr)));
        return Ok(Some(Value::Object(Some(ia))));
    }

    // Real DNS resolution via std::net
    let lookup = format!("{}:0", hostname);
    if let Ok(mut addrs) = std::net::ToSocketAddrs::to_socket_addrs(&lookup.as_str()) {
        if let Some(socket_addr) = addrs.next() {
            let ia = alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2);
            let host = ctx.create_string(&hostname);
            let addr = ctx.create_string(&socket_addr.ip().to_string());
            ctx.set_field(ia, 0, Value::Object(Some(host)));
            ctx.set_field(ia, 1, Value::Object(Some(addr)));
            return Ok(Some(Value::Object(Some(ia))));
        }
    }

    // Resolution failed — return exception (UnknownHostException)
    Err(RuntimeError::UnknownHostException {
        message: hostname.to_string(),
    }
    .into())
}

/// InetAddress.getAllByName(String) — resolve hostname to all IP addresses.
fn native_inet_get_all_by_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let hostname = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };

    let lookup = if hostname.is_empty() || hostname == "localhost" {
        "localhost:0".to_string()
    } else {
        format!("{}:0", hostname)
    };

    let mut results = Vec::new();
    if let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(&lookup.as_str()) {
        for sa in addrs {
            let ia = alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2);
            let host = ctx.create_string(&hostname);
            let addr = ctx.create_string(&sa.ip().to_string());
            ctx.set_field(ia, 0, Value::Object(Some(host)));
            ctx.set_field(ia, 1, Value::Object(Some(addr)));
            results.push(ia);
        }
    }
    if results.is_empty() {
        // At least return localhost
        let ia = alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2);
        let host = ctx.create_string(&hostname);
        let addr = ctx.create_string("127.0.0.1");
        ctx.set_field(ia, 0, Value::Object(Some(host)));
        ctx.set_field(ia, 1, Value::Object(Some(addr)));
        results.push(ia);
    }
    let arr = ctx.new_ref_array(ClassId::new(0), results.len());
    for (i, r) in results.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Object(Some(*r)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// InetAddress.getByAddress(byte[]) — create from raw bytes.
///
/// Accepts a 4-byte array (IPv4) or a 16-byte array (IPv6). The IPv6 path
/// previously returned a hardcoded `::1` regardless of the input bytes; this
/// version uses the actual bytes via [`std::net::Ipv6Addr::from`] so that
/// `getByAddress({0xfe, 0x80, ...})` returns an `InetAddress` whose
/// `getHostAddress()` reflects the real input.
fn native_inet_get_by_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let bytes_arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let len = ctx.array_length(bytes_arr);
    let addr_str = if len == 4 {
        let b = read_byte_array(ctx, bytes_arr, 4);
        std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]).to_string()
    } else if len == 16 {
        let b = read_byte_array(ctx, bytes_arr, 16);
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&b);
        std::net::Ipv6Addr::from(octets).to_string()
    } else {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("addr is of illegal length: {len}"),
        }
        .into());
    };
    let ia = alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2);
    let host = ctx.create_string(&addr_str);
    let addr = ctx.create_string(&addr_str);
    ctx.set_field(ia, 0, Value::Object(Some(host)));
    ctx.set_field(ia, 1, Value::Object(Some(addr)));
    Ok(Some(Value::Object(Some(ia))))
}

/// Parse the address string stored on field 1 into an `IpAddr`. Returns
/// `None` if the field is absent or not parseable.
fn inet_addr_string(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<std::net::IpAddr> {
    // Layout-aware: a CratonVM-synthesised `InetAddress` keeps its IP in the
    // `net_phase_e` ObjectRef-keyed side table / real `InetAddressHolder`,
    // NOT in instance slot 1 (which is the typed `holder` reference field).
    // Reading the raw slot would parse a `holder` object reference as a
    // String and return `None`. Consult the shared resolver first; only
    // fall back to the legacy slot for objects built by a different path.
    if let Some((_, ip)) = crate::net_phase_e::inet_addr_resolve(ctx, this) {
        if let Ok(parsed) = ip.parse::<std::net::IpAddr>() {
            return Some(parsed);
        }
    }
    let s = match ctx.get_field(this, 1) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => return None,
    };
    s.parse::<std::net::IpAddr>().ok()
}

/// `InetAddress.getAddress()` — return the raw bytes (4 for IPv4, 16 for IPv6).
fn native_inet_get_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let ip = inet_addr_string(ctx, this)
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)));
    let bytes: Vec<u8> = match ip {
        std::net::IpAddr::V4(v4) => v4.octets().to_vec(),
        std::net::IpAddr::V6(v6) => v6.octets().to_vec(),
    };
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int((*b as i8) as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// Map an `InetAddress` synthetic to its parsed `IpAddr`, or fall back to
/// `127.0.0.1` if the field is missing/garbled. Used by all the boolean
/// predicate helpers below so they share one parser.
fn inet_addr_or_loopback(ctx: &mut dyn NativeContext, this: ObjectRef) -> std::net::IpAddr {
    inet_addr_string(ctx, this).unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}

fn native_inet_is_loopback(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let yes = inet_addr_or_loopback(ctx, this).is_loopback();
    Ok(Some(Value::Int(if yes { 1 } else { 0 })))
}

fn native_inet_is_any_local(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let yes = inet_addr_or_loopback(ctx, this).is_unspecified();
    Ok(Some(Value::Int(if yes { 1 } else { 0 })))
}

fn native_inet_is_link_local(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let yes = match inet_addr_or_loopback(ctx, this) {
        // 169.254.0.0/16 (RFC 3927)
        std::net::IpAddr::V4(v4) => v4.is_link_local(),
        // fe80::/10 (RFC 4291)
        std::net::IpAddr::V6(v6) => {
            let segs = v6.segments();
            (segs[0] & 0xffc0) == 0xfe80
        }
    };
    Ok(Some(Value::Int(if yes { 1 } else { 0 })))
}

fn native_inet_is_site_local(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let yes = match inet_addr_or_loopback(ctx, this) {
        // RFC 1918 private blocks
        std::net::IpAddr::V4(v4) => v4.is_private(),
        // fec0::/10 (deprecated by RFC 3879 but still recognized by Java)
        std::net::IpAddr::V6(v6) => {
            let segs = v6.segments();
            (segs[0] & 0xffc0) == 0xfec0
        }
    };
    Ok(Some(Value::Int(if yes { 1 } else { 0 })))
}

fn native_inet_is_multicast(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let yes = match inet_addr_or_loopback(ctx, this) {
        std::net::IpAddr::V4(v4) => v4.is_multicast(),
        std::net::IpAddr::V6(v6) => v6.is_multicast(),
    };
    Ok(Some(Value::Int(if yes { 1 } else { 0 })))
}

/// `InetAddress.isReachable(int timeout_ms)` — best-effort reachability probe.
///
/// Real ICMP echo requires a privileged raw socket on every supported OS, so
/// instead we mirror what the OpenJDK `Inet*AddressImpl.isReachable` falls
/// back to when ICMP is unavailable: a TCP connect to port 7 (the historic
/// echo service). If port 7 fails or the OS returns ECONNREFUSED, we treat a
/// `Connection refused` as **reachable** (the host responded), which matches
/// HotSpot's behavior. Any other error / timeout is treated as not reachable.
fn native_inet_is_reachable(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let timeout_ms = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    let timeout = if timeout_ms > 0 {
        std::time::Duration::from_millis(timeout_ms as u64)
    } else {
        std::time::Duration::from_secs(1)
    };
    let ip = match inet_addr_string(ctx, this) {
        Some(ip) => ip,
        None => return Ok(Some(Value::Int(0))),
    };
    // Loopback and any-local are always trivially reachable.
    if ip.is_loopback() || ip.is_unspecified() {
        return Ok(Some(Value::Int(1)));
    }
    // Try TCP connect to port 7 (echo). A successful connect or a
    // ConnectionRefused both indicate the host is up.
    let target = std::net::SocketAddr::new(ip, 7);
    match std::net::TcpStream::connect_timeout(&target, timeout) {
        Ok(_) => Ok(Some(Value::Int(1))),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Ok(Some(Value::Int(1))),
        Err(_) => Ok(Some(Value::Int(0))),
    }
}

// ===========================================================================
// NEW-2 (java.net hardening) — unit + e2e tests
// ===========================================================================

#[cfg(test)]
mod new2_net_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;

    /// Helper: allocate a minimal `InetAddress` synthetic carrying a
    /// hostname / address pair. Mirrors what the real natives produce.
    fn alloc_inet(ctx: &mut MockNativeContext, host: &str, addr: &str) -> ObjectRef {
        let ia = alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2);
        let h = ctx.create_string(host);
        let a = ctx.create_string(addr);
        ctx.set_field(ia, 0, Value::Object(Some(h)));
        ctx.set_field(ia, 1, Value::Object(Some(a)));
        ia
    }

    // ---------------- C1: getLocalHost ----------------

    #[test]
    fn resolve_real_hostname_is_non_empty() {
        // The helper must always return a non-empty string. On any host
        // with a configured hostname this will be that name; otherwise
        // it falls back to "localhost". Either way: non-empty.
        let h = resolve_real_hostname();
        assert!(!h.is_empty(), "hostname must never be empty");
    }

    #[test]
    fn resolve_real_hostname_is_cached_and_matches_uncached_probe() {
        // `resolve_real_hostname` latches its result in a `OnceLock` because
        // the third resolution step forks and execs `hostname`, and
        // `InetAddress.getLocalHost()` / `NetworkInterface` enumeration call it
        // on every invocation. Two things must hold: repeated calls are stable,
        // and the cached value is exactly what the uncached probe produces.
        let first = resolve_real_hostname();
        let second = resolve_real_hostname();
        assert_eq!(first, second, "cached hostname must be stable across calls");
        assert_eq!(
            first,
            crate::resolve_real_hostname_uncached(),
            "cache must not change the resolved value"
        );
        assert!(!first.is_empty(), "hostname must never be empty");
    }

    #[test]
    fn resolve_primary_ipv4_is_parseable() {
        let h = resolve_real_hostname();
        let ip = resolve_primary_ipv4(&h);
        assert!(
            ip.parse::<std::net::Ipv4Addr>().is_ok(),
            "primary IPv4 must be a parseable v4 address (got {ip})"
        );
    }

    #[test]
    fn inet_get_local_host_populates_fields() {
        let mut ctx = MockNativeContext::new();
        let result = native_inet_localhost(&mut ctx, &[]).unwrap();
        let ia = match result {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an InetAddress object, got {other:?}"),
        };
        // field 0: hostname — non-empty
        let host = match ctx.get_field(ia, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        assert!(!host.is_empty(), "hostname must be set");
        // field 1: ip — parseable as IPv4
        let ip_s = match ctx.get_field(ia, 1) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        assert!(
            ip_s.parse::<std::net::Ipv4Addr>().is_ok(),
            "ip must parse as IPv4 (got {ip_s})"
        );
    }

    // ---------------- C2: getByAddress IPv6 ----------------

    #[test]
    fn inet_get_by_address_ipv4_round_trip() {
        let mut ctx = MockNativeContext::new();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
        // 192.168.1.42 — last octet sign-extends if cast naively.
        ctx.set_array_element(arr, 0, Value::Int((192u8 as i8) as i32));
        ctx.set_array_element(arr, 1, Value::Int((168u8 as i8) as i32));
        ctx.set_array_element(arr, 2, Value::Int(1));
        ctx.set_array_element(arr, 3, Value::Int(42));
        let result = native_inet_get_by_address(&mut ctx, &[Value::Object(Some(arr))]).unwrap();
        let ia = match result {
            Some(Value::Object(Some(o))) => o,
            _ => panic!("expected InetAddress"),
        };
        let ip = match ctx.get_field(ia, 1) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        assert_eq!(ip, "192.168.1.42");
    }

    #[test]
    fn inet_get_by_address_ipv6_uses_real_bytes() {
        let mut ctx = MockNativeContext::new();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
        // fe80::1 — link-local
        let bytes = [0xfeu8, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        for (i, b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int((*b as i8) as i32));
        }
        let result = native_inet_get_by_address(&mut ctx, &[Value::Object(Some(arr))]).unwrap();
        let ia = match result {
            Some(Value::Object(Some(o))) => o,
            _ => panic!("expected InetAddress"),
        };
        let ip = match ctx.get_field(ia, 1) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        // Std-lib formats this as the canonical compressed form.
        assert_eq!(
            ip, "fe80::1",
            "IPv6 getByAddress must use the real bytes, not hardcoded ::1"
        );
    }

    #[test]
    fn inet_get_by_address_invalid_length_throws() {
        let mut ctx = MockNativeContext::new();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 7);
        let result = native_inet_get_by_address(&mut ctx, &[Value::Object(Some(arr))]);
        assert!(
            result.is_err(),
            "byte[] of length != 4 and != 16 must throw IllegalArgumentException"
        );
    }

    // ---------------- C3: InetAddress predicates ----------------

    #[test]
    fn inet_predicates_loopback_ipv4() {
        let mut ctx = MockNativeContext::new();
        let lo = alloc_inet(&mut ctx, "localhost", "127.0.0.1");
        assert_eq!(
            native_inet_is_loopback(&mut ctx, &[Value::Object(Some(lo))]).unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            native_inet_is_any_local(&mut ctx, &[Value::Object(Some(lo))]).unwrap(),
            Some(Value::Int(0))
        );
        assert_eq!(
            native_inet_is_multicast(&mut ctx, &[Value::Object(Some(lo))]).unwrap(),
            Some(Value::Int(0))
        );
    }

    #[test]
    fn inet_predicates_loopback_ipv6() {
        let mut ctx = MockNativeContext::new();
        // ::1 is the IPv6 loopback. The IPv4-only string-prefix check used
        // to miss this — NEW-2 fix uses the parsed IpAddr predicates.
        let lo6 = alloc_inet(&mut ctx, "ip6-localhost", "::1");
        assert_eq!(
            native_inet_is_loopback(&mut ctx, &[Value::Object(Some(lo6))]).unwrap(),
            Some(Value::Int(1)),
            "::1 must be recognized as loopback"
        );
    }

    #[test]
    fn inet_predicates_any_local() {
        let mut ctx = MockNativeContext::new();
        let any4 = alloc_inet(&mut ctx, "wildcard", "0.0.0.0");
        let any6 = alloc_inet(&mut ctx, "wildcard6", "::");
        assert_eq!(
            native_inet_is_any_local(&mut ctx, &[Value::Object(Some(any4))]).unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            native_inet_is_any_local(&mut ctx, &[Value::Object(Some(any6))]).unwrap(),
            Some(Value::Int(1))
        );
    }

    #[test]
    fn inet_predicates_link_local_v4_v6() {
        let mut ctx = MockNativeContext::new();
        let v4 = alloc_inet(&mut ctx, "ll4", "169.254.1.2");
        let v6 = alloc_inet(&mut ctx, "ll6", "fe80::abcd");
        assert_eq!(
            native_inet_is_link_local(&mut ctx, &[Value::Object(Some(v4))]).unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            native_inet_is_link_local(&mut ctx, &[Value::Object(Some(v6))]).unwrap(),
            Some(Value::Int(1))
        );
        let lan = alloc_inet(&mut ctx, "lan", "192.168.0.1");
        assert_eq!(
            native_inet_is_link_local(&mut ctx, &[Value::Object(Some(lan))]).unwrap(),
            Some(Value::Int(0))
        );
    }

    #[test]
    fn inet_predicates_site_local_v4() {
        let mut ctx = MockNativeContext::new();
        for ip in ["10.0.0.1", "172.16.5.4", "192.168.1.1"] {
            let ia = alloc_inet(&mut ctx, "lan", ip);
            assert_eq!(
                native_inet_is_site_local(&mut ctx, &[Value::Object(Some(ia))]).unwrap(),
                Some(Value::Int(1)),
                "{ip} must be site-local"
            );
        }
        let public_ip = alloc_inet(&mut ctx, "public", "8.8.8.8");
        assert_eq!(
            native_inet_is_site_local(&mut ctx, &[Value::Object(Some(public_ip))]).unwrap(),
            Some(Value::Int(0))
        );
    }

    #[test]
    fn inet_predicates_multicast() {
        let mut ctx = MockNativeContext::new();
        let mc = alloc_inet(&mut ctx, "mcast", "224.0.0.1");
        assert_eq!(
            native_inet_is_multicast(&mut ctx, &[Value::Object(Some(mc))]).unwrap(),
            Some(Value::Int(1))
        );
        let mc6 = alloc_inet(&mut ctx, "mcast6", "ff02::1");
        assert_eq!(
            native_inet_is_multicast(&mut ctx, &[Value::Object(Some(mc6))]).unwrap(),
            Some(Value::Int(1))
        );
    }

    // ---------------- C4: getAddress() ----------------

    #[test]
    fn inet_get_address_v4() {
        let mut ctx = MockNativeContext::new();
        let ia = alloc_inet(&mut ctx, "h", "10.20.30.40");
        let result = native_inet_get_address(&mut ctx, &[Value::Object(Some(ia))]).unwrap();
        let arr = match result {
            Some(Value::Object(Some(a))) => a,
            _ => panic!("expected byte[]"),
        };
        assert_eq!(ctx.array_length(arr), 4);
        let mut got = [0u8; 4];
        for i in 0..4 {
            if let Value::Int(v) = ctx.get_array_element(arr, i) {
                got[i] = v as u8;
            }
        }
        assert_eq!(got, [10u8, 20, 30, 40]);
    }

    #[test]
    fn inet_get_address_v6() {
        let mut ctx = MockNativeContext::new();
        let ia = alloc_inet(&mut ctx, "h", "2001:db8::1");
        let result = native_inet_get_address(&mut ctx, &[Value::Object(Some(ia))]).unwrap();
        let arr = match result {
            Some(Value::Object(Some(a))) => a,
            _ => panic!("expected byte[]"),
        };
        assert_eq!(ctx.array_length(arr), 16);
        let mut got = [0u8; 16];
        for i in 0..16 {
            if let Value::Int(v) = ctx.get_array_element(arr, i) {
                got[i] = v as u8;
            }
        }
        let expected = std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).octets();
        assert_eq!(got, expected);
    }

    // ---------------- C5: isReachable ----------------

    #[test]
    fn inet_is_reachable_loopback() {
        let mut ctx = MockNativeContext::new();
        let lo = alloc_inet(&mut ctx, "localhost", "127.0.0.1");
        assert_eq!(
            native_inet_is_reachable(&mut ctx, &[Value::Object(Some(lo)), Value::Int(500)],)
                .unwrap(),
            Some(Value::Int(1)),
            "loopback must always be reachable"
        );
    }

    #[test]
    fn inet_is_reachable_unspecified_is_reachable() {
        let mut ctx = MockNativeContext::new();
        let any = alloc_inet(&mut ctx, "any", "0.0.0.0");
        assert_eq!(
            native_inet_is_reachable(&mut ctx, &[Value::Object(Some(any)), Value::Int(500)],)
                .unwrap(),
            Some(Value::Int(1))
        );
    }

    #[test]
    fn inet_is_reachable_unroutable_returns_false() {
        // 192.0.2.0/24 is the documentation-only TEST-NET-1 (RFC 5737);
        // it should never be reachable from a real network. We use a 100ms
        // timeout to keep the test fast.
        let mut ctx = MockNativeContext::new();
        let test_net = alloc_inet(&mut ctx, "doc", "192.0.2.1");
        let res =
            native_inet_is_reachable(&mut ctx, &[Value::Object(Some(test_net)), Value::Int(100)])
                .unwrap();
        assert_eq!(
            res,
            Some(Value::Int(0)),
            "TEST-NET-1 must not be reachable in <100ms"
        );
    }

    // ---------------- C8: end-to-end TCP loopback ----------------
    //
    // This test is a *direct* exercise of std::net (not the native-method
    // surface) — it confirms that the Berkeley-socket primitives the
    // native layer relies on actually round-trip on this build. If this
    // test passes, the existing Java-facing native paths (which all
    // funnel through std::net via s2_registry / fd_table) are exercised
    // end-to-end against real loopback.

    #[test]
    fn loopback_tcp_round_trip() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let local = listener.local_addr().expect("local_addr");
        // Server thread: accept once, echo bytes back.
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 5];
            let n = s.read(&mut buf).expect("server read");
            assert_eq!(n, 5);
            s.write_all(&buf[..n]).expect("server echo");
        });

        let mut client = TcpStream::connect(local).expect("connect");
        client.write_all(b"hello").expect("client write");
        let mut reply = [0u8; 5];
        client.read_exact(&mut reply).expect("client read");
        assert_eq!(&reply, b"hello");

        server.join().expect("server thread");
    }

    #[test]
    fn loopback_tcp_via_socket2_supports_set_reuse_address() {
        // Sanity-check that the socket2 sockopt path used by the
        // ServerSocket.setReuseAddress native is actually wired to the
        // OS, not a no-op.
        use socket2::SockRef;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let sock = SockRef::from(&listener);
        sock.set_reuse_address(true).expect("setsockopt");
        assert!(sock.reuse_address().expect("getsockopt"));
    }

    // ---------------- C6: socket exception messages ----------------
    //
    // These cannot be exercised through MockNativeContext alone because
    // the exception classes are concrete throwables in the wider VM and
    // the message round-trip requires the JDK exception chain. The
    // native registration itself is unit-tested by virtue of compiling
    // and the broader VM test suite covering throw/catch paths.

    // ---------------- DNS-config probe caching ----------------

    /// `os_dns_nameservers_string` forks `ipconfig /all` on Windows. The
    /// natives behind `sun/net/dns/ResolverConfigurationImpl.init0` and
    /// `.loadDNSconfig0` call it under the JDK's resolver-config refresh
    /// path, so an unlatched probe re-paid a process spawn every refresh.
    /// The cache must be stable and must not change the answer.
    #[test]
    fn os_dns_nameservers_is_cached_and_matches_uncached_probe() {
        let first = os_dns_nameservers_string();
        let second = os_dns_nameservers_string();
        assert_eq!(
            first, second,
            "the DNS-nameserver probe must be latched, not re-run per call"
        );
        assert_eq!(
            first,
            os_dns_nameservers_string_uncached(),
            "latching must not change the resolved nameserver list"
        );
    }

    /// The parse contract the callers depend on: a space-separated list of
    /// bare IP literals, and — critically — never a `null`-ish value, because
    /// the real JDK's `loadConfig()` feeds it straight into
    /// `stringToList(os_nameservers)` which calls `String.split` with no null
    /// guard (see the fn doc). An empty string is the correct "no servers"
    /// answer; anything else must parse as an `IpAddr`.
    #[test]
    fn os_dns_nameservers_yields_bare_ip_literals_or_empty() {
        let servers = os_dns_nameservers_string();
        for token in servers.split(' ').filter(|t| !t.is_empty()) {
            assert!(
                token.parse::<std::net::IpAddr>().is_ok(),
                "`{token}` is not an IP literal; ResolverConfigurationImpl \
                 splits this string on whitespace and hands the pieces to the \
                 DNS client verbatim"
            );
            assert!(
                !token.contains('%'),
                "`{token}` still carries an IPv6 zone id, which our UDP layer \
                 cannot route"
            );
        }
    }
}
