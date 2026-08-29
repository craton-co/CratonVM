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
///
/// Only a NON-EMPTY answer is latched. An empty one means "we could not read
/// the configuration", and that is not a conclusion worth remembering for the
/// life of the process: an empty resolver config is not inert — JNDI's
/// `DnsContextFactory.serversForUrls` falls back to the literal `"localhost"`,
/// so every lookup then queries 127.0.0.1:53 and times out. Latching that once
/// is exactly how a transient probe failure turned into a VM that could never
/// resolve a name again (see [`win_iphlpapi`]). Re-probing is cheap now that
/// the primary source is a direct API call rather than a process spawn.
pub(crate) fn os_dns_nameservers_string() -> String {
    static CACHED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    if let Ok(guard) = CACHED.lock() {
        if let Some(cached) = guard.as_ref() {
            return cached.clone();
        }
    }
    let probed = os_dns_nameservers_string_uncached();
    if !probed.is_empty() {
        if let Ok(mut guard) = CACHED.lock() {
            *guard = Some(probed.clone());
        }
    }
    probed
}

/// Windows IP Helper (`iphlpapi!GetNetworkParams`) binding — the same API the
/// real JDK's `loadDNSconfig0` uses, and the primary source for
/// [`os_dns_nameservers_string_uncached`].
///
/// This exists because the `ipconfig /all` fork below is **not reliable when
/// the VM has no attached console**: launched from a console-less parent (a
/// hidden-window process, a service, or any `ProcessStartInfo` with redirected
/// stdio — which is exactly how `apps/spring-boot-suite-runner` starts every
/// class), the child produced no usable output, the probe answered "no
/// nameservers", and because the answer is latched in a `OnceLock` the whole
/// VM ran with an empty resolver config for its entire life. JNDI's
/// `DnsContextFactory.serversForUrls` then falls back to the literal
/// `"localhost"` (see its `if (!platformServers.isEmpty())` branch), so every
/// lookup queried 127.0.0.1:53, where nothing listens, and timed out after the
/// full retry ladder (~15s). That is what made
/// `PropertiesMongoConnectionDetailsTests.protocolCanBeConfigured` /
/// `MongoAutoConfigurationTests.configuresProtocol` fail under the suite
/// runner while passing in an interactive shell.
///
/// A direct API call has none of those failure modes: no process spawn, no
/// console, no locale-dependent label text to parse.
#[cfg(target_os = "windows")]
mod win_iphlpapi {
    /// `IP_ADDRESS_STRING` / `IP_MASK_STRING` — a NUL-terminated dotted-quad.
    #[repr(C)]
    struct IpAddressString {
        string: [u8; 16],
    }

    /// `IP_ADDR_STRING` — singly-linked list node of addresses.
    #[repr(C)]
    struct IpAddrString {
        next: *mut IpAddrString,
        ip_address: IpAddressString,
        ip_mask: IpAddressString,
        context: u32,
    }

    /// `FIXED_INFO` (iptypes.h). `MAX_HOSTNAME_LEN`/`MAX_DOMAIN_NAME_LEN` are
    /// 128 and `MAX_SCOPE_ID_LEN` is 256, each declared with `+ 4` slack.
    #[repr(C)]
    struct FixedInfo {
        host_name: [u8; 132],
        domain_name: [u8; 132],
        current_dns_server: *mut IpAddrString,
        dns_server_list: IpAddrString,
        node_type: u32,
        scope_id: [u8; 260],
        enable_routing: u32,
        enable_proxy: u32,
        enable_dns: u32,
    }

    #[link(name = "iphlpapi")]
    extern "system" {
        fn GetNetworkParams(fixed_info: *mut FixedInfo, out_buf_len: *mut u32) -> u32;
    }

    const ERROR_SUCCESS: u32 = 0;
    const ERROR_BUFFER_OVERFLOW: u32 = 111;

    /// Configured IPv4 DNS servers, in order, de-duplicated. Empty on any
    /// failure — the caller falls back to the `ipconfig` parse.
    pub(super) fn nameservers() -> Vec<String> {
        // SAFETY: the two calls follow `GetNetworkParams`'s documented
        // size-probe-then-fill protocol. The buffer is over-allocated to the
        // length the API itself asked for and is `u64`-backed so the
        // `FixedInfo` cast is correctly aligned. Every pointer walked
        // afterwards comes from inside that same buffer, which outlives the
        // walk.
        unsafe {
            let mut len: u32 = 0;
            let probe = GetNetworkParams(std::ptr::null_mut(), &mut len);
            if probe != ERROR_BUFFER_OVERFLOW && probe != ERROR_SUCCESS {
                return Vec::new();
            }
            let bytes = (len as usize).max(std::mem::size_of::<FixedInfo>());
            let mut buf = vec![0u64; bytes.div_ceil(8)];
            let mut len = (buf.len() * 8) as u32;
            let info = buf.as_mut_ptr().cast::<FixedInfo>();
            if GetNetworkParams(info, &mut len) != ERROR_SUCCESS {
                return Vec::new();
            }
            let mut out: Vec<String> = Vec::new();
            let mut node: *const IpAddrString = &raw const (*info).dns_server_list;
            while !node.is_null() {
                let raw = &(*node).ip_address.string;
                let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                if let Ok(text) = std::str::from_utf8(&raw[..end]) {
                    let text = text.trim();
                    // 0.0.0.0 is what the API reports for "none configured".
                    if !text.is_empty()
                        && text != "0.0.0.0"
                        && text.parse::<std::net::IpAddr>().is_ok()
                        && !out.iter().any(|s| s == text)
                    {
                        out.push(text.to_string());
                    }
                }
                node = (*node).next;
            }
            out
        }
    }
}

/// The uncached probe behind [`os_dns_nameservers_string`]. Split out so the
/// `ipconfig /all` parsing stays testable without the `OnceLock` latch.
pub(crate) fn os_dns_nameservers_string_uncached() -> String {
    #[cfg(target_os = "windows")]
    {
        // Primary source: the IP Helper API (see [`win_iphlpapi`] for why the
        // `ipconfig` fork below cannot be the primary one). The fork is kept
        // as a fallback so a machine whose servers this API does not report
        // (e.g. IPv6-only configurations, which `GetNetworkParams` omits)
        // behaves no worse than before.
        let via_api = win_iphlpapi::nameservers();
        if !via_api.is_empty() {
            return via_api.join(" ");
        }
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
                if !line.contains('%') {
                    if let Some(host) = strip_zone_id(line) {
                        if host.parse::<std::net::IpAddr>().is_ok() {
                            servers.push(host.to_string());
                        }
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
                    if !value.contains('%') {
                        if let Some(host) = strip_zone_id(value) {
                            if host.parse::<std::net::IpAddr>().is_ok() {
                                servers.push(host.to_string());
                            }
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
        "getLoopbackAddress",
        "()Ljava/net/InetAddress;",
        |ctx, _args| {
            let ia = try_alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2)?;
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

/// JDK-ONLY-LAYOUT: does `this` have the synthetic six-slot **URL** layout that
/// [`url_parse`]'s raw `URL_FIELD_*` writes assume?
///
/// A real `java.net.URI` does not, and `native_uri_init` calls `url_parse` with
/// exactly that receiver. Measured 2026-08-04 with
/// `CRATONVM_DBG=overlay,overlay-all` from `SocksSocketImpl.connect`:
/// `URL_FIELD_PORT` (slot 2) put an `Int` port on the real `URI.authority`, a
/// reference, and `URL_FIELD_FULL` (slot 5) put a `String` on the real
/// `URI.port`, an `int`. Both writes are also pointless there — `native_uri_init`
/// calls [`uri_store_named`] straight afterwards, which stores the same
/// information under the names a real `URI` actually declares.
///
/// Detected by asking for a field name only the real classes declare:
/// `java.net.URI` declares `scheme`, `java.net.URL` declares `protocol`, and a
/// VM-fabricated stub has generated placeholders and declares neither. Asking by
/// *name* rather than by field count is deliberate — a count test cannot
/// separate these layouts, which is how an earlier version of a sibling guard in
/// `lang_invoke.rs` shipped completely inert.
fn has_synthetic_url_layout(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(this);
    !ctx.declared_fields(class_id)
        .iter()
        .any(|f| !f.is_static && (f.name == "scheme" || f.name == "protocol"))
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
        // Raw slots only on OUR layout — the same guard the generic path below
        // carries, which this arm was missing. `native_uri_init` reaches here
        // with a REAL `java.net.URI` receiver for every `file:` URI (a
        // `Path.toUri()`, a code source, a Spring Boot launcher URL), where
        // `URL_FIELD_PORT` (slot 2) put `Int(-1)` on `URI.authority` — the one
        // `set_field` row for `java/net/URI` in the 2026-08-10 census, and the
        // reason `getAuthority()` answered null on a real image.
        if has_synthetic_url_layout(ctx, this) {
            ctx.set_field(this, URL_FIELD_PROTOCOL, Value::Object(Some(proto_obj)));
            ctx.set_field(this, URL_FIELD_HOST, Value::Object(Some(host_empty)));
            ctx.set_field(this, URL_FIELD_PORT, Value::Int(-1));
            ctx.set_field(this, URL_FIELD_PATH, Value::Object(Some(path_obj)));
            ctx.set_field(this, URL_FIELD_QUERY, Value::Object(query_obj));
        } else {
            // Real layout: the same values under the names the real class
            // declares. `set_field_by_name` is a no-op for a name the class
            // does not declare, so one list serves `java.net.URL` (`protocol`)
            // and `java.net.URI` (`scheme`) alike.
            ctx.set_field_by_name(this, "protocol", Value::Object(Some(proto_obj)));
            ctx.set_field_by_name(this, "scheme", Value::Object(Some(proto_obj)));
            ctx.set_field_by_name(this, "host", Value::Object(Some(host_empty)));
            ctx.set_field_by_name(this, "port", Value::Int(-1));
        }
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

    // Parse: protocol://host[:port][/path][?query][#ref]
    let (protocol, rest) = if let Some(pos) = url_str.find("://") {
        (&url_str[..pos], &url_str[pos + 3..])
    } else {
        ("", url_str)
    };
    // Split the fragment off FIRST, exactly as the `file:` fast path above
    // already does. Without this the `#ref` stayed glued to the end of the
    // path/query, so `getPath()` on `http://h/p#sec` answered `"/p#sec"` and
    // `getRef()` had nothing to read.
    let (rest, ref_part) = if let Some(pos) = rest.find('#') {
        (&rest[..pos], Some(&rest[pos + 1..]))
    } else {
        (rest, None)
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
    let ref_obj = ref_part
        .filter(|fragment| !fragment.is_empty())
        .map(|fragment| ctx.create_string(fragment));

    // Raw slots only on OUR layout — see `has_synthetic_url_layout`. On a real
    // `java.net.URI` these six put a port on `authority` and a `String` on
    // `port`. The by-name writes below, and `uri_store_named` in the URI
    // callers, carry the same information to fields that exist.
    if has_synthetic_url_layout(ctx, this) {
        ctx.set_field(this, URL_FIELD_PROTOCOL, Value::Object(Some(proto_obj)));
        ctx.set_field(this, URL_FIELD_HOST, Value::Object(Some(host_obj)));
        ctx.set_field(this, URL_FIELD_PORT, Value::Int(port));
        ctx.set_field(this, URL_FIELD_PATH, Value::Object(Some(path_obj)));
        ctx.set_field(this, URL_FIELD_QUERY, Value::Object(query_obj));
        ctx.set_field(this, URL_FIELD_FULL, Value::Object(Some(full_obj)));
    } else {
        // Real layout: the same six values, under the names the real class
        // declares. `java.net.URL` names them protocol/host/port; `java.net.URI`
        // names the first `scheme`. `set_field_by_name` is a no-op for a name
        // the class does not declare, so one list serves both.
        ctx.set_field_by_name(this, "protocol", Value::Object(Some(proto_obj)));
        ctx.set_field_by_name(this, "scheme", Value::Object(Some(proto_obj)));
        ctx.set_field_by_name(this, "host", Value::Object(Some(host_obj)));
        ctx.set_field_by_name(this, "port", Value::Int(port));
    }
    ctx.set_field_by_name(this, "file", Value::Object(Some(file_obj)));
    ctx.set_field_by_name(this, "path", Value::Object(Some(path_obj)));
    ctx.set_field_by_name(this, "query", Value::Object(query_obj));
    // Only meaningful on a layout with a named `ref` field (a real
    // `java.net.URL`); the no-name synthetic stubs recover the fragment from
    // the cached full URL in `net_phase_e`'s `getRef`. There is no free raw
    // slot for it in the 6-slot synthetic layout.
    ctx.set_field_by_name(this, "ref", Value::Object(ref_obj));
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
    let uri = try_alloc_concurrent_synthetic(ctx, "java/net/URI", 6)?;
    url_parse(ctx, uri, &full);
    // `url_parse` writes the URL-shaped names; a `java.net.URI` additionally
    // needs `string`, `schemeSpecificPart` and `authority`, and those are what
    // `uri_raw_string` and `getAuthority` read on a real layout.
    net_phase_e::uri_publish_named(ctx, uri, &full, None);
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
    let string_hash = |value: &str| {
        value.bytes().fold(0i32, |hash, byte| {
            hash.wrapping_mul(31).wrapping_add(byte as i32)
        })
    };
    let protocol = url_str_field(ctx, this, "protocol", URL_FIELD_PROTOCOL).unwrap_or_default();
    // URLStreamHandler's file-handler contract is component based, not the
    // hash of `toExternalForm()`: protocol + host + port + file + ref.  The
    // generic external-form hash happened to preserve equality but broke a
    // jar URL handler which delegates its inner `file:` URL to URL.hashCode.
    // Keep the existing canonical external-form behavior for other protocols
    // while matching the JDK's file-URL rule exactly (including port -1).
    if protocol.eq_ignore_ascii_case("file") {
        let host = url_str_field(ctx, this, "host", URL_FIELD_HOST).unwrap_or_default();
        let port = match ctx.get_field_by_name(this, "port") {
            Value::Int(port) => port,
            _ => match ctx.get_field(this, URL_FIELD_PORT) {
                Value::Int(port) => port,
                _ => -1,
            },
        };
        let file = url_str_field(ctx, this, "file", URL_FIELD_PATH).unwrap_or_default();
        let reference = match ctx.get_field_by_name(this, "ref") {
            Value::Object(Some(reference)) => ctx.read_string(reference).unwrap_or_default(),
            _ => String::new(),
        };
        let hash = string_hash(&protocol)
            .wrapping_add(string_hash(&host.to_ascii_lowercase()))
            .wrapping_add(port)
            .wrapping_add(string_hash(&file))
            .wrapping_add(string_hash(&reference));
        return Ok(Some(Value::Int(hash)));
    }
    Ok(Some(Value::Int(string_hash(&url_external_form(ctx, this)))))
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
    // `authority`, `query` and `fragment` under the names the real class
    // declares. They were the components with neither a named write here nor a
    // registered accessor, so on a real image `URI.getAuthority()` ran real
    // bytecode against a field nothing had written and answered **null** for
    // every URI whose authority is not just its host — measured 2026-08-10:
    // `new URI("http://user:pw@example.com:8080/a/b?q=1#frag")` answered null
    // where HotSpot answers `user:pw@example.com:8080`. `getAuthority` /
    // `getRawAuthority` are registered natives now as well; this write is what
    // keeps the OBJECT right for the real bytecode that reads the field
    // directly (`URI.equals`, `URI.hashCode`, `URI.toString`).
    let (_, authority, _, query, fragment) = net_phase_e::uri_split(full);
    // A URI with no `//authority` has NO port, full stop. `url_parse` ran on
    // this same receiver a moment ago and, having no `//` to anchor on, split
    // the whole string on its LAST colon: `urn:isbn:0451450523` came out as
    // host `urn:isbn` and port `451450523`, which it then wrote to the named
    // `port` field. `net_phase_e`'s `getPort` returns that field verbatim
    // whenever it is positive, so the port survived all the way out.
    // MEASURED 2026-08-17 against Temurin 25.0.3+9-LTS — HotSpot answers -1 for
    // every one of these and we answered the trailing digits:
    //
    //   new URI("urn:isbn:0451450523").getPort()   HotSpot -1, ours 451450523
    //   new URI("a:1234").getPort()                HotSpot -1, ours 1234
    //   new URI("mailto:a@b.com:25").getPort()     HotSpot -1, ours 25
    //
    // Writing the sentinel back is enough: with a non-positive field `getPort`
    // falls through to the raw-string parse, which sees no authority and
    // answers -1 on its own.
    if authority.is_none() {
        ctx.set_field_by_name(this, "port", Value::Int(-1));
    }
    let mut store_named = |ctx: &mut dyn NativeContext, name: &str, v: &Option<String>| {
        if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
            let s = ctx.create_string(v);
            ctx.set_field_by_name(this, name, Value::Object(Some(s)));
        }
    };
    store_named(ctx, "authority", &authority);
    store_named(ctx, "query", &query);
    store_named(ctx, "fragment", &fragment);

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
    uri_first_char_fault(s).map(|(index, _)| index)
}

/// Why [`uri_first_char_fault`] stopped at a character.
///
/// The JDK reports these as two DIFFERENT `URISyntaxException` reasons at the
/// same index, and the difference is visible in `getMessage()`. MEASURED
/// 2026-08-17 against Temurin 25.0.3+9-LTS -- every one of these is
/// `Malformed escape pair`, never `Illegal character in <component>`:
///
/// ```text
///   new URI("http://h/a%")       Malformed escape pair at index 10
///   new URI("http://h/a%2")      Malformed escape pair at index 10
///   new URI("http://h/a%zz")     Malformed escape pair at index 10
///   new URI("http://h/p?q=%2")   Malformed escape pair at index 13
///   new URI("http://h/p#f%2")    Malformed escape pair at index 12
///   new URI("http://h%2/p")      Malformed escape pair at index 8
///   new URI("%2")                Malformed escape pair at index 0
///   new URI("mailto:a%2")        Malformed escape pair at index 8
/// ```
///
/// The component name is NOT part of it: the same reason is used in the path,
/// the query, the fragment, the authority and a bare relative reference. It is
/// the JDK's `Parser.scanEscape`, which runs before any component-specific
/// character check, so whichever offence comes FIRST left-to-right wins --
/// also measured: `http://h/a b%2` is `Illegal character in path at index 10`
/// (the space) while `http://h/a%2 b` is `Malformed escape pair at index 10`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UriCharFault {
    /// The character is outside the URI character set for its component.
    Illegal,
    /// A `%` that does not begin a `%` HEX HEX triple.
    MalformedEscape,
}

/// The first character `java.net.URI`'s single-string parser would refuse, and
/// why. [`uri_first_illegal_index`] is the index-only view of this, kept for
/// the `URI.create` caller that only needs the position.
pub(crate) fn uri_first_char_fault(s: &str) -> Option<(usize, UriCharFault)> {
    let bytes = s.as_bytes();
    // Inside an IPv6 literal a `%` is a scope-id separator, NOT the start of an
    // escape triple -- the JDK switches the authority to `L_SERVER_PERCENT` as
    // soon as the authority contains a `]`. MEASURED 2026-08-17: HotSpot
    // ACCEPTS `new URI("http://[::1%eth0]/p")` and `new URI("http://[::1%zz]/p")`
    // and reports host `[::1%eth0]`, while we refused both with
    // `Illegal character in authority at index 11`. Refusing a URI the JDK
    // accepts is the worse half of this bug, so the exemption is deliberately
    // narrow: only the bytes strictly between the authority's `[` and its `]`.
    let bracket = uri_ipv6_bracket_span(s);
    for (i, c) in s.char_indices() {
        let u = c as u32;
        if u < 0x20 || u == 0x7f {
            return Some((i, UriCharFault::Illegal));
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
            if bracket.is_some_and(|(open, close)| open < i && i < close) {
                continue;
            }
            let valid_escape = bytes.get(i + 1).copied().map(is_ascii_hex_digit) == Some(true)
                && bytes.get(i + 2).copied().map(is_ascii_hex_digit) == Some(true);
            if !valid_escape {
                return Some((i, UriCharFault::MalformedEscape));
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
            return Some((i, UriCharFault::Illegal));
        }
    }
    None
}

/// The `[` … `]` span of the authority's IPv6 literal, as absolute byte
/// offsets into `s` (`open` is the `[`, `close` the `]` or the end of the
/// authority when the bracket is never closed).
///
/// Anchored on `://` for the same reason the closing-bracket check in
/// [`native_uri_init`] is: that is the only authority shape the URI natives
/// currently police, and widening it to bare `//authority` references is a
/// separate change with its own blast radius.
fn uri_ipv6_bracket_span(s: &str) -> Option<(usize, usize)> {
    let open_auth = s.find("://")? + 3;
    let auth_end = s[open_auth..]
        .find(['/', '?', '#'])
        .map(|r| open_auth + r)
        .unwrap_or(s.len());
    let br = s[open_auth..auth_end].find('[').map(|i| open_auth + i)?;
    let close = s[br + 1..auth_end]
        .find(']')
        .map(|i| br + 1 + i)
        .unwrap_or(auth_end);
    Some((br, close))
}

/// A refusal from the transcribed `java.net.URI` parser: HotSpot's `reason`
/// string exactly as `URI$Parser` spells it, plus the index it reports.
///
/// `index: None` is the JDK's one-argument `fail(String reason)` site. It
/// builds the `URISyntaxException` through the two-argument constructor, whose
/// index is -1, and `URISyntaxException.getMessage()` then omits the
/// `" at index N"` clause entirely. MEASURED 2026-08-17 -- the only URI parse
/// failure in the whole family that carries no index:
/// `new URI("http://[::1%]/p")` is `scope id expected: http://[::1%]/p`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UriParseFail {
    pub(crate) reason: &'static str,
    pub(crate) index: Option<usize>,
}

impl UriParseFail {
    fn at(reason: &'static str, index: usize) -> Self {
        UriParseFail {
            reason,
            index: Some(index),
        }
    }
}

/// `java.net.URI$Parser`'s IPv6 scanner, transcribed.
///
/// This is a direct transcription of `parseIPv6Reference` / `scanHexPost` /
/// `scanHexSeq` / `scanIPv4Address` / `takeIPv4Address` / `scanByte` from
/// `java.base/java/net/URI.java` (Temurin 25.0.3+9-LTS `src.zip`), because the
/// indices these report cannot be derived from the grammar -- they are
/// wherever that particular scan happened to stop. All positions are ABSOLUTE
/// offsets into the whole URI string, which is what the exception carries.
struct Ipv6Scanner<'a> {
    b: &'a [u8],
    /// The JDK's `ipv6byteCount`: two per hex group, four per embedded IPv4.
    byte_count: i32,
}

impl<'a> Ipv6Scanner<'a> {
    fn at_char(&self, p: usize, n: usize, c: u8) -> bool {
        p < n && self.b[p] == c
    }

    fn at_double_colon(&self, p: usize, n: usize) -> bool {
        n.saturating_sub(p) >= 2 && self.b[p] == b':' && self.b[p + 1] == b':'
    }

    fn scan_char(&self, p: usize, n: usize, c: u8) -> usize {
        if self.at_char(p, n, c) {
            p + 1
        } else {
            p
        }
    }

    fn scan_hex(&self, p: usize, n: usize) -> usize {
        let mut q = p;
        while q < n && self.b[q].is_ascii_hexdigit() {
            q += 1;
        }
        q
    }

    fn scan_digits(&self, p: usize, n: usize) -> usize {
        let mut q = p;
        while q < n && self.b[q].is_ascii_digit() {
            q += 1;
        }
        q
    }

    fn scan_digits_and_dots(&self, p: usize, n: usize) -> usize {
        let mut q = p;
        while q < n && (self.b[q].is_ascii_digit() || self.b[q] == b'.') {
            q += 1;
        }
        q
    }

    /// `scanByte` — a run of decimal digits whose value fits in a byte. Returns
    /// `p` unchanged when the run is too large, which is how the caller detects
    /// the failure and where it reports it.
    fn scan_byte(&self, p: usize, n: usize) -> usize {
        let q = self.scan_digits(p, n);
        if q <= p {
            return q;
        }
        let mut i = p;
        while i < q && self.b[i] == b'0' {
            i += 1;
        }
        let significant = q - i;
        if significant < 3 {
            return q;
        }
        if significant > 3 {
            return p;
        }
        let value = std::str::from_utf8(&self.b[i..q])
            .ok()
            .and_then(|d| d.parse::<u32>().ok())
            .unwrap_or(u32::MAX);
        if value > 255 {
            p
        } else {
            q
        }
    }

    /// `scanIPv4Address`. `Ok(None)` is the JDK's `-1` ("not an IPv4 address").
    fn scan_ipv4(
        &self,
        start: usize,
        n: usize,
        strict: bool,
    ) -> Result<Option<usize>, UriParseFail> {
        let m = self.scan_digits_and_dots(start, n);
        if m <= start || (strict && m != n) {
            return Ok(None);
        }
        let mut p = start;
        let mut q = start;
        loop {
            q = self.scan_byte(p, m);
            if q <= p {
                break;
            }
            p = q;
            q = self.scan_char(p, m, b'.');
            if q <= p {
                break;
            }
            p = q;
            q = self.scan_byte(p, m);
            if q <= p {
                break;
            }
            p = q;
            q = self.scan_char(p, m, b'.');
            if q <= p {
                break;
            }
            p = q;
            q = self.scan_byte(p, m);
            if q <= p {
                break;
            }
            p = q;
            q = self.scan_char(p, m, b'.');
            if q <= p {
                break;
            }
            p = q;
            q = self.scan_byte(p, m);
            if q <= p {
                break;
            }
            p = q;
            if q < m {
                break;
            }
            return Ok(Some(q));
        }
        if strict {
            return Err(UriParseFail::at("Malformed IPv4 address", q));
        }
        Ok(None)
    }

    /// `takeIPv4Address`. `expected` is the already-composed
    /// `"Expected " + expected` reason, since `failExpecting` only ever
    /// prefixes that one word.
    fn take_ipv4(
        &mut self,
        start: usize,
        n: usize,
        expected: &'static str,
    ) -> Result<usize, UriParseFail> {
        match self.scan_ipv4(start, n, true)? {
            Some(p) if p > start => Ok(p),
            _ => Err(UriParseFail::at(expected, start)),
        }
    }

    /// `scanHexSeq`. `Ok(None)` is the JDK's `-1`.
    fn scan_hex_seq(&mut self, start: usize, n: usize) -> Result<Option<usize>, UriParseFail> {
        let mut p = start;
        let mut q = self.scan_hex(p, n);
        if q <= p {
            return Ok(None);
        }
        if self.at_char(q, n, b'.') {
            // Beginning of an IPv4 address, not a hex group.
            return Ok(None);
        }
        if q > p + 4 {
            return Err(UriParseFail::at(
                "IPv6 hexadecimal digit sequence too long",
                p,
            ));
        }
        self.byte_count += 2;
        p = q;
        while p < n {
            if !self.at_char(p, n, b':') {
                break;
            }
            if self.at_char(p + 1, n, b':') {
                break;
            }
            p += 1;
            q = self.scan_hex(p, n);
            if q <= p {
                return Err(UriParseFail::at("Expected digits for an IPv6 address", p));
            }
            if self.at_char(q, n, b'.') {
                p -= 1;
                break;
            }
            if q > p + 4 {
                return Err(UriParseFail::at(
                    "IPv6 hexadecimal digit sequence too long",
                    p,
                ));
            }
            self.byte_count += 2;
            p = q;
        }
        Ok(Some(p))
    }

    /// `scanHexPost`.
    fn scan_hex_post(&mut self, start: usize, n: usize) -> Result<usize, UriParseFail> {
        let mut p = start;
        if p == n {
            return Ok(p);
        }
        match self.scan_hex_seq(p, n)? {
            Some(q) if q > p => {
                p = q;
                if self.at_char(p, n, b':') {
                    p += 1;
                    p = self.take_ipv4(p, n, "Expected hex digits or IPv4 address")?;
                    self.byte_count += 4;
                }
            }
            _ => {
                p = self.take_ipv4(p, n, "Expected hex digits or IPv4 address")?;
                self.byte_count += 4;
            }
        }
        Ok(p)
    }

    /// `parseIPv6Reference` over `b[start..n]` (the bracket BODY, without the
    /// brackets themselves).
    fn parse_reference(&mut self, start: usize, n: usize) -> Result<usize, UriParseFail> {
        let mut p = start;
        let mut compressed_zeros = false;
        match self.scan_hex_seq(p, n)? {
            Some(q) if q > p => {
                p = q;
                if self.at_double_colon(p, n) {
                    compressed_zeros = true;
                    p = self.scan_hex_post(p + 2, n)?;
                } else if self.at_char(p, n, b':') {
                    p = self.take_ipv4(p + 1, n, "Expected IPv4 address")?;
                    self.byte_count += 4;
                }
            }
            _ => {
                if self.at_double_colon(p, n) {
                    compressed_zeros = true;
                    p = self.scan_hex_post(p + 2, n)?;
                }
            }
        }
        if p < n {
            return Err(UriParseFail::at("Malformed IPv6 address", start));
        }
        if self.byte_count > 16 {
            return Err(UriParseFail::at("IPv6 address too long", start));
        }
        if !compressed_zeros && self.byte_count < 16 {
            return Err(UriParseFail::at("IPv6 address too short", start));
        }
        if compressed_zeros && self.byte_count == 16 {
            return Err(UriParseFail::at("Malformed IPv6 address", start));
        }
        Ok(p)
    }
}

/// The refusal `java.net.URI`'s server-based authority parser would raise for a
/// bracketed (IPv6-literal) authority, or `None` if it would accept it.
///
/// Only bracketed authorities are policed here, and that is deliberate: the
/// JDK falls back to a REGISTRY-based authority when the server-based parse
/// fails, which is why `new URI("http://h:-5/p")` is perfectly legal (it just
/// answers `getHost() == null`, `getPort() == -1`). An authority containing a
/// `]` is not a legal registry name, so there is no fallback and the parse
/// failure is fatal -- that asymmetry is the whole reason these refusals exist
/// only inside brackets. MEASURED 2026-08-17 against Temurin 25.0.3+9-LTS:
///
/// ```text
///   http://[abc]/p                 IPv6 address too short at index 8
///   http://[abcd]/p                IPv6 address too short at index 8
///   http://[1:2:3:4:5:6:7]/p       IPv6 address too short at index 8
///   http://[abcde]/p               IPv6 hexadecimal digit sequence too long at index 8
///   http://[12345::1]/p            IPv6 hexadecimal digit sequence too long at index 8
///   http://[v7.abc]/p              Malformed IPv6 address at index 8
///   http://[1.2.3.4]/p             Malformed IPv6 address at index 8
///   http://[g::1]/p                Malformed IPv6 address at index 8
///   http://[:1]/p                  Malformed IPv6 address at index 8
///   http://[%eth0]/p               Malformed IPv6 address at index 8
///   http://[::1:2:3:4:5:6:7:8]/p   Malformed IPv6 address at index 8
///   http://[1:2:3:4:5:6:7:8:9]/p   IPv6 address too long at index 8
///   http://[1:]/p                  Expected digits for an IPv6 address at index 10
///   http://[::1:]/p                Expected digits for an IPv6 address at index 12
///   http://[::256.1.1.1]/p         Malformed IPv4 address at index 10
///   http://[::1.2.3]/p             Malformed IPv4 address at index 15
///   http://[::1.2.3.400]/p         Malformed IPv4 address at index 16
///   http://[::1.2.3.4.5]/p         Malformed IPv4 address at index 17
///   http://[::ffff:1.2.3.999]/p    Malformed IPv4 address at index 21
///   http://[::1.2.3.4x]/p          Expected hex digits or IPv4 address at index 10
///   http://[::1%]/p                scope id expected            (NO index)
///   http://[::1]]/p                Expected port number at index 12
///   http://[::1]x/p                Expected port number at index 12
///   http://[::1]:x/p               Illegal character in port number at index 13
///   http://[::1]:-5/p              Illegal character in port number at index 13
///   http://[::1]:+80/p             Illegal character in port number at index 13
///   http://[::1]:8x/p              Illegal character in port number at index 14
///   http://u@[::1]:x/p             Illegal character in port number at index 15
///   http://[::1]:99999999999/p     Malformed port number at index 13
///   http://[::1]:2147483648/p      Malformed port number at index 13
/// ```
///
/// and these are ACCEPTED, so the check must not fire for them:
/// `[::1]`, `[::]`, `[fe80::1]`, `[1:2:3:4:5:6:7:8]`, `[1:2:3:4:5:6:1.2.3.4]`,
/// `[::1.2.3.4]`, `[::ffff:1.2.3.4]`, `[1234::1]`, `[::1%eth0]`, `[::1%zz]`,
/// `[::1%25]`, `[::1]:`, `[::1]:0`, `[::1]:007`, `[::1]:2147483647`.
///
/// Returns `None` for the unclosed / empty-bracket shapes, which the older
/// `Expected closing bracket for IPv6 address` check in [`native_uri_init`]
/// already refuses at the same indices.
pub(crate) fn uri_ipv6_authority_fail(s: &str) -> Option<UriParseFail> {
    let b = s.as_bytes();
    let open_auth = s.find("://")? + 3;
    let auth_end = s[open_auth..]
        .find(['/', '?', '#'])
        .map(|r| open_auth + r)
        .unwrap_or(s.len());
    // `parseServer` takes the userinfo off first: `scan(p, n, "/?#", "@")`, so
    // the host begins after the FIRST `@` in the authority, if there is one.
    let host_start = s[open_auth..auth_end]
        .find('@')
        .map(|i| open_auth + i + 1)
        .unwrap_or(open_auth);
    if b.get(host_start) != Some(&b'[') {
        return None;
    }
    let body_start = host_start + 1;
    let close = s[body_start..auth_end].find(']').map(|i| body_start + i)?;
    if close == body_start {
        return None;
    }

    let mut scanner = Ipv6Scanner { b, byte_count: 0 };
    match s[body_start..close].find('%').map(|i| body_start + i) {
        // `int r = scan(p, q, "%")` only counts as a scope id when it advanced,
        // i.e. when the `%` is not the very first byte of the body. A leading
        // `%` stays part of the address and fails there instead.
        Some(pct) if pct > body_start => {
            if let Err(fail) = scanner.parse_reference(body_start, pct) {
                return Some(fail);
            }
            if pct + 1 == close {
                return Some(UriParseFail {
                    reason: "scope id expected",
                    index: None,
                });
            }
            // checkChars(r + 1, q, L_SCOPE_ID, …) — scope_id = alphanum | "_" | "."
            for i in pct + 1..close {
                let c = b[i];
                if !(c.is_ascii_alphanumeric() || c == b'_' || c == b'.') {
                    return Some(UriParseFail::at("Illegal character in scope id", i));
                }
            }
        }
        _ => {
            if let Err(fail) = scanner.parse_reference(body_start, close) {
                return Some(fail);
            }
        }
    }

    // After the `]`: either the end of the authority, or `:` <port>. The
    // authority never contains `/`, so the JDK's `q = scan(p, n, "/")` is just
    // the end of the authority.
    let mut p = close + 1;
    if p < auth_end && b[p] == b':' {
        p += 1;
        if auth_end > p {
            if let Some(bad) = (p..auth_end).find(|i| !b[*i].is_ascii_digit()) {
                return Some(UriParseFail::at("Illegal character in port number", bad));
            }
            if s[p..auth_end].parse::<i32>().is_err() {
                return Some(UriParseFail::at("Malformed port number", p));
            }
        }
        p = auth_end;
    }
    if p < auth_end {
        return Some(UriParseFail::at("Expected port number", p));
    }
    None
}

/// Build the `java.net.URISyntaxException` for a [`UriParseFail`], choosing the
/// two- or three-argument constructor exactly as the JDK's `fail` overloads do.
/// [`uri_syntax_exception`] for callers outside this module — `URI`'s natives
/// live in `net_phase_e` and need the same exception shape.
pub(crate) fn uri_syntax_exception_pub(
    ctx: &mut dyn NativeContext,
    input: &str,
    fail: &UriParseFail,
) -> Option<MethodCallFailed> {
    uri_syntax_exception(ctx, input, fail)
}

fn uri_syntax_exception(
    ctx: &mut dyn NativeContext,
    input: &str,
    fail: &UriParseFail,
) -> Option<MethodCallFailed> {
    let input_obj = ctx.create_string(input);
    let reason_obj = ctx.create_string(fail.reason);
    let built = match fail.index {
        Some(index) => ctx.new_object_initialized(
            "java/net/URISyntaxException",
            "(Ljava/lang/String;Ljava/lang/String;I)V",
            &[
                Value::Object(Some(input_obj)),
                Value::Object(Some(reason_obj)),
                Value::Int(index as i32),
            ],
        ),
        None => ctx.new_object_initialized(
            "java/net/URISyntaxException",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            &[
                Value::Object(Some(input_obj)),
                Value::Object(Some(reason_obj)),
            ],
        ),
    };
    match built {
        Ok(Some(Value::Object(Some(exc)))) => Some(MethodCallFailed::ExceptionThrown(exc)),
        _ => None,
    }
}

/// The index at which `java.net.URI` would throw
/// `URISyntaxException("Expected closing bracket for IPv6 address", index)`.
///
/// An IPv6 literal in the authority must be `[` <non-empty> `]`. MEASURED
/// 2026-08-13 (/tmp/W.java) — both of these are `URISyntaxException` on HotSpot
/// and were once accepted here, i.e. a malformed URI parsed clean:
///
/// ```text
///   new URI("http://[::1/")  Expected closing bracket for IPv6 address at index 11
///   new URI("http://[]/")    Expected closing bracket for IPv6 address at index 8
/// ```
///
/// The reported index is where the address parse stopped: the end of the
/// authority when the `]` is missing, and the position just past `[` when the
/// body is empty. `http://[fe80::1]/` and `http://[::1]:80/` stay legal.
///
/// **This is a FUNCTION because the rule had two doors and only one of them
/// enforced it.** It was written inline in `native_uri_init`, so `URI.create`
/// — which is `new URI(str)` with the checked exception translated — could not
/// reach it: `URI.create("http://[::1/a")` returned a URI where the constructor
/// threw. MEASURED by `apps/probes/UriRecompositionSweep.java`.
pub(crate) fn uri_closing_bracket_fail_index(s: &str) -> Option<usize> {
    let open = s.find("://").map(|i| i + 3)?;
    let auth_end = s[open..]
        .find(['/', '?', '#'])
        .map(|r| open + r)
        .unwrap_or(s.len());
    let auth = &s[open..auth_end];
    let br = auth.find('[')?;
    let abs_br = open + br;
    match s[abs_br + 1..auth_end].find(']') {
        None => Some(auth_end),
        Some(0) => Some(abs_br + 1),
        Some(_) => None,
    }
}

/// The `UriParseFail` `java.net.URI.parseServerAuthority()` raises, or `None`
/// when the authority really is server-based (or there is none at all, which
/// the JDK treats as success).
///
/// MEASURED against HotSpot 25.0.4+7 — the index is the offending character's
/// own position, and the two reasons are distinct:
///
/// ```text
///   URI.create("http://host:x/a").parseServerAuthority()
///     Illegal character in port number at index 12
///   URI.create("http://ho_st/a").parseServerAuthority()
///     Illegal character in hostname at index 9
///   URI.create("http://host:99999/a")  accepted — there is NO range check
///   URI.create("urn:x:y")              accepted — opaque, no authority
///   URI.create("//host/a")             accepted — authority without a scheme
/// ```
///
/// The host character set is deliberately NARROW rather than a full hostname
/// grammar: it refuses what is outside `[A-Za-z0-9.-]`, which is what separates
/// a registry-based authority from a server-based one for every shape measured,
/// and refusing a URI the JDK accepts is the worse half of this bug (the same
/// reasoning `uri_first_char_fault`'s bracket exemption records). A bracketed
/// IPv6 host is skipped entirely — the constructor has already validated it.
pub(crate) fn uri_server_authority_fail(s: &str) -> Option<UriParseFail> {
    // The authority begins after `//`, with or without a scheme.
    let after_scheme = match s.find("://") {
        Some(i) => i + 3,
        None if s.starts_with("//") => 2,
        None => return None,
    };
    let auth_end = s[after_scheme..]
        .find(['/', '?', '#'])
        .map(|r| after_scheme + r)
        .unwrap_or(s.len());
    if auth_end == after_scheme {
        return None;
    }
    // `parseServer` takes the userinfo off first: `scan(p, n, "/?#", "@")`.
    let host_start = s[after_scheme..auth_end]
        .find('@')
        .map(|i| after_scheme + i + 1)
        .unwrap_or(after_scheme);
    if s.as_bytes().get(host_start) == Some(&b'[') {
        return None;
    }
    // The port is whatever follows the LAST `:` of the host region.
    let host_end = s[host_start..auth_end]
        .rfind(':')
        .map(|i| host_start + i)
        .unwrap_or(auth_end);
    for (i, c) in s[host_start..host_end].char_indices() {
        if !(c.is_ascii_alphanumeric() || c == '.' || c == '-') {
            return Some(UriParseFail::at(
                "Illegal character in hostname",
                host_start + i,
            ));
        }
    }
    if host_end < auth_end {
        for (i, c) in s[host_end + 1..auth_end].char_indices() {
            if !c.is_ascii_digit() {
                return Some(UriParseFail::at(
                    "Illegal character in port number",
                    host_end + 1 + i,
                ));
            }
        }
    }
    None
}

/// Returns the index at which `java.net.URI`'s parser would throw
/// `URISyntaxException("Expected authority", index)`, or `None`.
///
/// `Parser.parseHierarchical` scans the authority region and then branches
/// three ways, and only the third is a failure:
///
/// ```text
///   p += 2;                          // past the "//"
///   int q = scan(p, n, "", "/?#");
///   if (q > p)      parseAuthority(p, q);   // a real authority
///   else if (q < n) { /* DEVIATION: empty authority before a non-empty
///                        path, query or fragment is ALLOWED */ }
///   else            failExpecting("authority", p);
/// ```
///
/// So an empty authority is legal exactly when something follows it. `http://`
/// and `file://` and the bare relative `//` have nothing following, and the JDK
/// rejects all three; `http:///a`, `http://?q` and `http://#f` are accepted.
/// This VM accepted the first three as well, and then answered for them — the
/// probe row `[file://] path` was `""` against HotSpot's
/// `THREW java.net.URISyntaxException`, and twenty-one more accessors beside
/// it, three specs deep: **66 of `UriRecompositionSweep`'s 111 differing rows.**
///
/// The index is the position just past the `//`, which is what
/// `failExpecting("authority", p)` reports.
pub(crate) fn uri_expected_authority_fail_index(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    // Where the hierarchical part starts: after `scheme:` if there is a
    // well-formed scheme, else at 0 for a relative reference.
    let start = {
        let mut end = None;
        for (i, c) in b.iter().enumerate() {
            if *c == b':' {
                if i > 0
                    && b[0].is_ascii_alphabetic()
                    && b[..i]
                        .iter()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
                {
                    end = Some(i);
                }
                break;
            }
            if !(c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.')) {
                break;
            }
        }
        end.map(|i| i + 1).unwrap_or(0)
    };
    if b.len() < start + 2 || b[start] != b'/' || b[start + 1] != b'/' {
        return None;
    }
    let p = start + 2;
    let q = s[p..]
        .find(['/', '?', '#'])
        .map(|r| p + r)
        .unwrap_or(s.len());
    if q == p && q == s.len() {
        Some(p)
    } else {
        None
    }
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
    // MEASURED 2026-08-13 (/tmp/T.java): `new URI(null)` is a helpful NPE
    // naming the JDK's own private field, not a URISyntaxException and not a
    // silently-empty URI. The `_ => String::new()` arm below turned a null
    // argument into the empty string, which is a LEGAL relative URI -- so the
    // constructor succeeded and handed back a usable object.
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some(
                "Cannot invoke \"String.length()\" because \"this.input\" is null".into(),
            ),
        }
        .into());
    }
    // The argument object is pinned HERE, before anything below allocates.
    // It is restored verbatim at the end of this function (see the note
    // there); a raw `ObjectRef` re-read from `args` after the parse would be
    // a from-space address under a moving young collection.
    let raw_arg = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let raw_pin = raw_arg.map(|o| ctx.pin_native_root(o));
    let url_str = match raw_arg {
        Some(o) => ctx.read_string(o).unwrap_or_default(),
        None => String::new(),
    };
    // Read once, BEFORE the parse allocates: the component correction at the
    // end of this function needs the units and the object may move.
    let raw_units: Vec<u16> = match raw_arg {
        Some(o) => ctx.read_string_units(o).unwrap_or_default(),
        None => Vec::new(),
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
    // An IPv6 literal in the authority must be `[` <non-empty> `]`. MEASURED
    // 2026-08-13 (/tmp/W.java) -- both of these are URISyntaxException on
    // HotSpot and were ACCEPTED here, i.e. a malformed URI parsed clean:
    //
    //   new URI("http://[::1/")  Expected closing bracket for IPv6 address at index 11
    //   new URI("http://[]/")    Expected closing bracket for IPv6 address at index 8
    //
    // The reported index is where the address parse stopped: the end of the
    // authority when the `]` is missing, and the position just past `[` when the
    // body is empty. `http://[fe80::1]/` and `http://[::1]:80/` stay legal.
    {
        let bad = uri_closing_bracket_fail_index(&url_str);
        {
            if let Some(pos) = bad {
                let input = ctx.create_string(&url_str);
                let reason = ctx.create_string("Expected closing bracket for IPv6 address");
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
        }
    }

    let strict_uri_chars = crate::nbflags().uri_strict_chars;
    let illegal = if strict_uri_chars {
        uri_first_char_fault(&url_str)
    } else {
        url_str
            .char_indices()
            .find(|(_, c)| (*c as u32) < 0x20 || (*c as u32) == 0x7f)
            .map(|(i, _)| (i, UriCharFault::Illegal))
    };
    if let Some((pos, UriCharFault::MalformedEscape)) = illegal {
        // A `%` that does not begin a `%` HEX HEX triple is its OWN reason in
        // the JDK, produced by `Parser.scanEscape` before any component-specific
        // character check runs — see [`UriCharFault`] for the sixteen measured
        // rows. We reported the component name here, so `getMessage()` read
        // `Illegal character in path at index 10` where HotSpot says
        // `Malformed escape pair at index 10`.
        if let Some(exc) = uri_syntax_exception(
            ctx,
            &url_str,
            &UriParseFail::at("Malformed escape pair", pos),
        ) {
            return Err(exc);
        }
    }
    if let Some((pos, UriCharFault::Illegal)) = illegal {
        let input = ctx.create_string(&url_str);
        // The JDK names the COMPONENT the offending character sits in, not the
        // URI as a whole. MEASURED 2026-08-13 (/tmp/W2.java) -- five distinct
        // names, and the boundaries are the delimiters themselves:
        //
        //   htt<p://h/      Illegal character in scheme name at index 3
        //   //auth<x/p      Illegal character in authority   at index 6
        //   http://h/pa<th  Illegal character in path        at index 11
        //   http://h/p?q<1  Illegal character in query       at index 12
        //   http://h/p#f<1  Illegal character in fragment    at index 12
        //
        // A relative "/pa<th" with no scheme and no authority is still "path",
        // so the component is decided by position, not by what the URI has.
        let component = {
            let frag = url_str.find('#');
            let query = url_str.find('?').filter(|q| frag.is_none_or(|f| *q < f));
            let scheme_end = url_str.find(':').filter(|c| {
                url_str[..*c]
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || "+-.".contains(ch))
                    && url_str[..*c].starts_with(|ch: char| ch.is_ascii_alphabetic())
            });
            let auth_start = url_str.find("//").map(|s| s + 2);
            let auth_stop = auth_start.map(|s| {
                url_str[s..]
                    .find(['/', '?', '#'])
                    .map(|r| s + r)
                    .unwrap_or(url_str.len())
            });
            if frag.is_some_and(|f| pos > f) {
                "fragment"
            } else if query.is_some_and(|q| pos > q) {
                "query"
            } else if scheme_end.is_some_and(|c| pos < c) {
                "scheme name"
            } else if auth_start.is_some_and(|s| pos >= s) && auth_stop.is_some_and(|e| pos < e) {
                "authority"
            } else {
                "path"
            }
        };
        let reason = ctx.create_string(&format!("Illegal character in {component}"));
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
    // Reject a bracketed authority whose IPv6 literal, scope id or port the
    // JDK's server-based parser would refuse. This runs AFTER the generic
    // character check on purpose: HotSpot scans the authority against its
    // character set first, so `http://[abc]<>/p` is an illegal-character
    // failure and only a character-clean authority reaches `parseServer`.
    // See [`uri_ipv6_authority_fail`] for the thirty measured rows.
    if strict_uri_chars {
        if let Some(fail) = uri_ipv6_authority_fail(&url_str) {
            if let Some(exc) = uri_syntax_exception(ctx, &url_str, &fail) {
                return Err(exc);
            }
        }
    }
    // Reject `scheme://` and a bare `//` — an empty authority with NOTHING
    // after it. See [`uri_expected_authority_fail_index`] for the JDK's
    // three-way branch and the sixty-six probe rows this was worth.
    if let Some(pos) = uri_expected_authority_fail_index(&url_str) {
        if let Some(exc) =
            uri_syntax_exception(ctx, &url_str, &UriParseFail::at("Expected authority", pos))
        {
            return Err(exc);
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
    // Hand back the text we were GIVEN, not a copy of the decode.
    //
    // `url_str` is a Rust `String`, which cannot hold an unpaired surrogate,
    // so every store built from it substitutes U+FFFD. MEASURED on both VMs
    // at `e9bed7b89`, with the source built from a `char[]` so no
    // constant-pool interning is involved:
    //
    //   new URI("http://h/a<U+D800>b").toString()
    //     HotSpot   charAt(10)=d800, and toString() == the argument
    //     CratonVM  charAt(10)=fffd, and toString() != the argument
    //
    // The LENGTH was right on both, which is why this survived: exactly one
    // code unit differed, so nothing that measures size or splits on ASCII
    // delimiters ever noticed. `url_str` stays for the PARSING above, which
    // splits on ASCII delimiters and is unaffected by the substitution.
    //
    // Only the verbatim text is restored. The parsed components are still
    // built from the decode and still carry U+FFFD — see this file's
    // nomination in `G61-1`; fixing those needs component slicing by code
    // unit, which is a different and much larger change.
    //
    // Slot 6 is written ONLY on our synthetic layout: on a real
    // `java.net.URI`, slot 6 is `path`, and writing the whole URI there would
    // corrupt it. Same rule, and the same reason, as the JDK-ONLY-LAYOUT
    // guard in `phases_early.rs`.
    if let (Some(pin), Some(raw0)) = (raw_pin, raw_arg) {
        let raw_ref = ctx.read_native_pin(pin, raw0);
        ctx.set_field_by_name(this, "string", Value::Object(Some(raw_ref)));
        if net_phase_e::uri_has_synthetic_layout(ctx, this) {
            ctx.set_field(this, 6, Value::Object(Some(raw_ref)));
        }
        // G75-1 N1 — rewrite the text-carrying COMPONENT fields from the raw
        // units, using the same splitter the getters use.
        //
        // `url_parse` above derived them from `url_str`, which came through
        // `read_string` and cannot hold an unpaired surrogate. The getters
        // prefer these fields over their own parse when the field is non-empty,
        // so a lossy field silently won over an exact parse — which is why
        // converting the getters alone fixed the multi-argument constructor and
        // not this one.
        //
        // This is a correction pass rather than a conversion of `url_parse`
        // because it reuses ONE splitter: the same
        // `uri_select_raw_path_units`/`uri_query_units`/`uri_fragment_units`
        // the accessors call. Converting the 195-line parser would have been a
        // second spelling of the same rule for the three components that need
        // it and no change at all for the ASCII-constrained rest.
        //
        // Only written when the splitter answers `Some`: an OPAQUE URI has a
        // null path and must keep whatever `url_parse` decided, and `getPath`
        // decides opacity from the raw text anyway.
        if let Some(path_u) = net_phase_e::uri_select_raw_path_units(&raw_units) {
            let obj = ctx.create_string_from_units(&path_u);
            ctx.set_field_by_name(this, "path", Value::Object(Some(obj)));
        }
        if let Some(q) = net_phase_e::uri_query_units(&raw_units) {
            let obj = ctx.create_string_from_units(&q);
            ctx.set_field_by_name(this, "query", Value::Object(Some(obj)));
        }
        if let Some(f) = net_phase_e::uri_fragment_units(&raw_units) {
            let obj = ctx.create_string_from_units(&f);
            ctx.set_field_by_name(this, "fragment", Value::Object(Some(obj)));
        }
        ctx.unpin_native_roots(pin);
    }
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
    let uri = try_alloc_concurrent_synthetic(ctx, "java/net/URI", 6)?;
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
    let ia = try_alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2)?;
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
        let ia = try_alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2)?;
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
        let ia = try_alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2)?;
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
            let ia = try_alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2)?;
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
            let ia = try_alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2)?;
            let host = ctx.create_string(&hostname);
            let addr = ctx.create_string(&sa.ip().to_string());
            ctx.set_field(ia, 0, Value::Object(Some(host)));
            ctx.set_field(ia, 1, Value::Object(Some(addr)));
            results.push(ia);
        }
    }
    if results.is_empty() {
        // At least return localhost
        let ia = try_alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2)?;
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
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Helper: allocate a minimal `InetAddress` synthetic carrying a
    /// hostname / address pair. Mirrors what the real natives produce.
    fn alloc_inet(ctx: &mut MockNativeContext, host: &str, addr: &str) -> ObjectRef {
        let ia = try_alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2).unwrap();
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

    /// `os_dns_nameservers_string` probes the platform resolver configuration
    /// on Windows. The natives behind `sun/net/dns/ResolverConfigurationImpl.init0`
    /// and `.loadDNSconfig0` call it under the JDK's resolver-config refresh
    /// path, so an unlatched probe re-paid the whole cost every refresh.
    /// The cache must be stable and must not change the answer.
    ///
    /// Only a non-empty answer is latched — see `os_dns_nameservers_string`'s
    /// own doc for why remembering "no nameservers" is actively harmful. This
    /// test holds either way: with servers configured both calls return the
    /// latched list; with none, both return the same empty string.
    #[test]
    fn os_dns_nameservers_is_cached_and_matches_uncached_probe() {
        let first = os_dns_nameservers_string();
        let second = os_dns_nameservers_string();
        assert_eq!(
            first, second,
            "a resolved DNS-nameserver list must be latched, not re-run per call"
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

    // ---------------------------------------------------------------------
    // G14-1: the `new URI(String)` refusal surface.
    //
    // Every expectation below is a TRANSCRIPTION of a row measured on
    // Temurin 25.0.3+9-LTS on 2026-08-17, not a derivation from the grammar.
    // The reason strings and the indices are what `URISyntaxException`'s
    // `getReason()` / `getIndex()` actually answered; where they look
    // arbitrary (an index pointing at the first non-digit for one port
    // failure and at the port's first character for another) that is because
    // they ARE arbitrary — they are wherever the JDK's scan stopped.
    // ---------------------------------------------------------------------

    /// The offending character's index, and whether the JDK would blame a
    /// malformed escape triple rather than the component's character set.
    #[track_caller]
    fn assert_escape_fault(uri: &str, index: usize) {
        assert_eq!(
            uri_first_char_fault(uri),
            Some((index, UriCharFault::MalformedEscape)),
            "`{uri}` must be a `Malformed escape pair` at index {index}"
        );
    }

    #[track_caller]
    fn assert_illegal_char(uri: &str, index: usize) {
        assert_eq!(
            uri_first_char_fault(uri),
            Some((index, UriCharFault::Illegal)),
            "`{uri}` must be an illegal-character refusal at index {index}"
        );
    }

    #[test]
    fn uri_malformed_escape_pairs_are_their_own_reason() {
        // MEASURED: all sixteen are `Malformed escape pair at index N`, never
        // `Illegal character in <component>` — and the component makes no
        // difference to the reason, only to the index.
        assert_escape_fault("http://h/a%", 10);
        assert_escape_fault("http://h/a%2", 10);
        assert_escape_fault("http://h/a%A", 10);
        assert_escape_fault("http://h/a%zz", 10);
        assert_escape_fault("http://h/a%2z", 10);
        assert_escape_fault("http://h/a%z2", 10);
        assert_escape_fault("http://h/a%GG", 10);
        assert_escape_fault("http://h/a%2G", 10);
        assert_escape_fault("http://h/a%%20", 10);
        assert_escape_fault("http://h/p?q=%2", 13);
        assert_escape_fault("http://h/p?q=%zz", 13);
        assert_escape_fault("http://h/p#f%2", 12);
        assert_escape_fault("http://h/p#f%zz", 12);
        assert_escape_fault("http://h%2/p", 8);
        assert_escape_fault("http://h%zz/p", 8);
        assert_escape_fault("http://u%2@h/p", 8);
        assert_escape_fault("%2", 0);
        assert_escape_fault("a%zzb", 1);
        assert_escape_fault("mailto:a%2", 8);
    }

    #[test]
    fn uri_well_formed_escapes_are_accepted() {
        for uri in [
            "http://h/a%20b",
            "http://h/a%41b",
            "http://h/a%C3%A9",
            "http://h/a%c3%a9",
            "http://h/a%FF",
            "http://h/a%ff%fe",
        ] {
            assert_eq!(uri_first_char_fault(uri), None, "`{uri}` is legal");
        }
    }

    #[test]
    fn uri_first_offence_wins_left_to_right() {
        // MEASURED — the JDK scans forward and reports whichever offence it
        // reaches first, so the SAME two characters swap the reason when their
        // order swaps.
        assert_illegal_char("http://h/a b%2", 10); // the space, at 10
        assert_escape_fault("http://h/a%2 b", 10); // the `%`, at 10
        assert_illegal_char("http://h/a<b%2", 10);
        assert_escape_fault("http://h/a%2<b", 10);
        assert_escape_fault("http://h/a%2%2", 10);
    }

    #[test]
    fn percent_inside_an_ipv6_literal_is_a_scope_id_not_an_escape() {
        // MEASURED — HotSpot ACCEPTS all three. We used to refuse the first two
        // with `Illegal character in authority`, which is the worse half of the
        // bug: refusing input the JDK accepts.
        for uri in [
            "http://[::1%eth0]/p",
            "http://[::1%zz]/p",
            "http://[::1%25]/p",
        ] {
            assert_eq!(
                uri_first_char_fault(uri),
                None,
                "`{uri}` must survive the character scan"
            );
        }
        // …but only INSIDE the brackets. A bare `%` anywhere else is still an
        // escape triple that has to be well formed.
        assert_escape_fault("http://[::1]/a%2", 14);
        assert_escape_fault("http://h%2/p", 8);
    }

    #[track_caller]
    fn assert_ipv6_fail(uri: &str, reason: &str, index: Option<usize>) {
        let got = uri_ipv6_authority_fail(uri).map(|f| (f.reason, f.index));
        assert_eq!(
            got,
            Some((reason, index)),
            "`{uri}` must be refused with `{reason}` at {index:?}"
        );
    }

    #[test]
    fn ipv6_literal_bodies_match_the_jdk_parser() {
        assert_ipv6_fail("http://[abc]/p", "IPv6 address too short", Some(8));
        assert_ipv6_fail("http://[abcd]/p", "IPv6 address too short", Some(8));
        assert_ipv6_fail("http://[1]/p", "IPv6 address too short", Some(8));
        assert_ipv6_fail("http://[12]/p", "IPv6 address too short", Some(8));
        assert_ipv6_fail("http://[1:2]/p", "IPv6 address too short", Some(8));
        assert_ipv6_fail("http://[1:2:3]/p", "IPv6 address too short", Some(8));
        assert_ipv6_fail(
            "http://[1:2:3:4:5:6:7]/p",
            "IPv6 address too short",
            Some(8),
        );
        assert_ipv6_fail(
            "http://[abcde]/p",
            "IPv6 hexadecimal digit sequence too long",
            Some(8),
        );
        assert_ipv6_fail(
            "http://[12345]/p",
            "IPv6 hexadecimal digit sequence too long",
            Some(8),
        );
        assert_ipv6_fail(
            "http://[12345::1]/p",
            "IPv6 hexadecimal digit sequence too long",
            Some(8),
        );
        assert_ipv6_fail("http://[v7.abc]/p", "Malformed IPv6 address", Some(8));
        assert_ipv6_fail("http://[V7.abc]/p", "Malformed IPv6 address", Some(8));
        assert_ipv6_fail("http://[vz.abc]/p", "Malformed IPv6 address", Some(8));
        assert_ipv6_fail("http://[v.abc]/p", "Malformed IPv6 address", Some(8));
        assert_ipv6_fail("http://[v7.]/p", "Malformed IPv6 address", Some(8));
        assert_ipv6_fail("http://[1.2.3.4]/p", "Malformed IPv6 address", Some(8));
        assert_ipv6_fail("http://[g::1]/p", "Malformed IPv6 address", Some(8));
        assert_ipv6_fail("http://[:1]/p", "Malformed IPv6 address", Some(8));
        assert_ipv6_fail("http://[%eth0]/p", "Malformed IPv6 address", Some(8));
        assert_ipv6_fail(
            "http://[::1:2:3:4:5:6:7:8]/p",
            "Malformed IPv6 address",
            Some(8),
        );
        assert_ipv6_fail(
            "http://[1:2:3:4:5:6:7:8:9]/p",
            "IPv6 address too long",
            Some(8),
        );
        assert_ipv6_fail(
            "http://[1:2:3:4:5:6:7:1.2.3.4]/p",
            "IPv6 address too long",
            Some(8),
        );
        assert_ipv6_fail(
            "http://[1:]/p",
            "Expected digits for an IPv6 address",
            Some(10),
        );
        assert_ipv6_fail(
            "http://[::1:]/p",
            "Expected digits for an IPv6 address",
            Some(12),
        );
    }

    #[test]
    fn embedded_ipv4_indices_are_wherever_the_scan_stopped() {
        // These four indices are the reason this is a transcription: each one
        // is a different position for the same reason string.
        assert_ipv6_fail("http://[::256.1.1.1]/p", "Malformed IPv4 address", Some(10));
        assert_ipv6_fail("http://[::1.2.3]/p", "Malformed IPv4 address", Some(15));
        assert_ipv6_fail("http://[::1.2.3.400]/p", "Malformed IPv4 address", Some(16));
        assert_ipv6_fail("http://[::1.2.3.4.5]/p", "Malformed IPv4 address", Some(17));
        assert_ipv6_fail(
            "http://[::ffff:1.2.3.999]/p",
            "Malformed IPv4 address",
            Some(21),
        );
        assert_ipv6_fail(
            "http://[::1.2.3.4x]/p",
            "Expected hex digits or IPv4 address",
            Some(10),
        );
    }

    #[test]
    fn ipv6_scope_id_rules() {
        // The ONE refusal in the family that carries no index at all.
        assert_ipv6_fail("http://[::1%]/p", "scope id expected", None);
        for uri in [
            "http://[::1%eth0]/p",
            "http://[::1%zz]/p",
            "http://[::1%25]/p",
        ] {
            assert_eq!(uri_ipv6_authority_fail(uri), None, "`{uri}` is legal");
        }
    }

    #[test]
    fn port_after_an_ipv6_literal_is_digits_only() {
        assert_ipv6_fail(
            "http://[::1]:x/p",
            "Illegal character in port number",
            Some(13),
        );
        assert_ipv6_fail(
            "http://[::1]:-5/p",
            "Illegal character in port number",
            Some(13),
        );
        assert_ipv6_fail(
            "http://[::1]:+80/p",
            "Illegal character in port number",
            Some(13),
        );
        // The index moves to the first NON-digit, not the start of the port.
        assert_ipv6_fail(
            "http://[::1]:8x/p",
            "Illegal character in port number",
            Some(14),
        );
        assert_ipv6_fail(
            "http://[::1]:80x80/p",
            "Illegal character in port number",
            Some(15),
        );
        // …and userinfo shifts every index along with it.
        assert_ipv6_fail(
            "http://u@[::1]:x/p",
            "Illegal character in port number",
            Some(15),
        );
        // All digits but out of `int` range is a DIFFERENT reason, reported at
        // the start of the port rather than at any particular digit.
        assert_ipv6_fail(
            "http://[::1]:99999999999/p",
            "Malformed port number",
            Some(13),
        );
        assert_ipv6_fail(
            "http://[::1]:2147483648/p",
            "Malformed port number",
            Some(13),
        );
    }

    #[test]
    fn anything_but_a_colon_after_the_closing_bracket_expects_a_port() {
        assert_ipv6_fail("http://[::1]]/p", "Expected port number", Some(12));
        assert_ipv6_fail("http://[::1]x/p", "Expected port number", Some(12));
    }

    #[test]
    fn well_formed_ipv6_authorities_are_left_alone() {
        for uri in [
            "http://[::1]/p",
            "http://[::]/p",
            "http://[fe80::1]/p",
            "http://[1:2:3:4:5:6:7:8]/p",
            "http://[1234::1]/p",
            "http://[1:2:3:4:5:6:1.2.3.4]/p",
            "http://[::1.2.3.4]/p",
            "http://[::ffff:1.2.3.4]/p",
            "http://[::1]:80/p",
            "http://[::1]:0/p",
            "http://[::1]:007/p",
            "http://[::1]:2147483647/p",
            "http://[::1]:/p",
            "http://[::1]",
            "http://[::1]?q",
            "http://[::1]#f",
            "http://u@[::1]/p",
            "http://u@[::1]:80/p",
        ] {
            assert_eq!(uri_ipv6_authority_fail(uri), None, "`{uri}` is legal");
        }
    }

    #[test]
    fn unclosed_and_empty_brackets_stay_with_the_older_check() {
        // `native_uri_init`'s `Expected closing bracket for IPv6 address` check
        // already refuses these at the right indices; the IPv6 body scanner
        // must not also claim them, or the reason would change.
        for uri in ["http://[/", "http://[]/p", "http://[::1/"] {
            assert_eq!(uri_ipv6_authority_fail(uri), None, "`{uri}` is not ours");
        }
    }

    #[test]
    fn an_authority_less_uri_has_no_port_to_find() {
        // The three rows `uri_store_named`'s sentinel write exists for: with no
        // `//`, `url_parse` reads the LAST colon as a port delimiter.
        for uri in ["urn:isbn:0451450523", "a:1234", "mailto:a@b.com:25"] {
            let (_, authority, _, _, _) = crate::net_phase_e::uri_split(uri);
            assert!(
                authority.is_none(),
                "`{uri}` has no authority, so `getPort()` must be -1"
            );
        }
        // …and a URI that DOES have one keeps it.
        let (_, authority, _, _, _) = crate::net_phase_e::uri_split("http://h:80/p");
        assert_eq!(authority.as_deref(), Some("h:80"));
    }
}
