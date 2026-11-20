#[allow(unused_imports)]
use super::*;

pub fn uri_raw_string(ctx: &dyn NativeContext, uri: ObjectRef) -> String {
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
    if !uri_has_synthetic_layout(ctx, uri) {
        // `--real-jdk`: a real `URI` whose `string` cache is still null is one
        // real bytecode built field by field -- `resolve`, `normalize` and
        // `relativize` all do, and `toString()` fills the cache lazily. With
        // the `URI` bridges retired those are ordinary values here, and every
        // native reader that asks this function (`Path.of(URI)`,
        // `HttpRequest.newBuilder(URI)`, the proxy selector) would otherwise
        // read "" for them. Rebuild the text the way the JDK's own
        // `defineString` does.
        //
        // Only when the cache is NULL, not merely empty: `new URI("")` caches
        // "" and is exactly that text, while this VM's own `URI(String)`
        // native (live again under `CRATONVM_UNRETIRE_NATIVE_SHADOW`) writes
        // "" into `scheme`/`authority` too, which `defineString` would render
        // as "://". MEASURED on the unretire arm of `SbrW1Uri` before this
        // guard: six rows moved off the base binary's answers.
        if jdk_uri_model(&*ctx)
            && matches!(ctx.get_field_by_name(uri, "string"), Value::Object(None))
        {
            if let Some(s) = uri_define_string(ctx, uri) {
                return s;
            }
        }
        return String::new();
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

// ---------------------------------------------------------------------------
// G75-1 N1 — the URI accessors, in code units
// ---------------------------------------------------------------------------
//
// Every `getPath`/`getQuery`/`getFragment` here RE-PARSES the raw URI text; the
// component fields are only consulted as a fallback. So the loss was not in
// `url_parse` at all — each getter read the raw string with `read_string`,
// which cannot hold an unpaired UTF-16 surrogate, and re-lost the unit on its
// own. Converting the parse alone would have fixed nothing observable.
//
// The delimiters that decide URI structure are ALL ASCII (`:`/`/`/`?`/`#`), so
// scanning units for them is the same logic with a different index type. What
// matters is that there is still ONE rule: the units functions below are the
// implementations, and the `&str` spellings that already existed are now thin
// wrappers over them. A `&str` can never hold a lone surrogate, so encoding one
// to units and back is exact — the wrappers lose nothing they did not already
// lack, and the two spellings cannot drift because there is only one body.

/// Index of the first occurrence of an ASCII byte in a unit slice.
pub fn u_find(hay: &[u16], needle: u8) -> Option<usize> {
    hay.iter().position(|&c| c == u16::from(needle))
}

/// Index of the first occurrence of ANY of the given ASCII bytes.
pub fn u_find_any(hay: &[u16], set: &[u8]) -> Option<usize> {
    hay.iter()
        .position(|&c| set.iter().any(|&b| u16::from(b) == c))
}

pub fn u_starts_with(hay: &[u16], prefix: &str) -> bool {
    let p: Vec<u16> = prefix.encode_utf16().collect();
    hay.starts_with(&p)
}

pub fn u_hex(c: u16) -> Option<u8> {
    char::from_u32(u32::from(c))
        .and_then(|c| c.to_digit(16))
        .map(|d| d as u8)
}

/// [`uri_raw_string`] without the loss.
///
/// Reuses `uri_raw_string_object`, which already encodes the same field search
/// order (`string`, then slots 6 and 5), so the two cannot disagree about WHICH
/// string is the raw one.
pub fn uri_raw_units(ctx: &dyn NativeContext, uri: ObjectRef) -> Vec<u16> {
    if let Some(o) = uri_raw_string_object(ctx, uri) {
        if let Some(u) = ctx.read_string_units(o) {
            if !u.is_empty() {
                return u;
            }
        }
    }
    // `uri_raw_string` has one further fallback (slot 0, gated on a `:/`
    // heuristic) that the object finder does not model. Defer to it rather than
    // restate the heuristic here: those receivers are synthetic-layout `file:`
    // URLs whose text is ASCII, so nothing is lost by going through `&str`.
    uri_raw_string(ctx, uri).encode_utf16().collect()
}

/// The `//authority` component of `uri`, or `None` when it has none.
///
/// The raw string is authoritative — it is what every sibling accessor in this
/// registrar parses — with the real class's own `authority` field as the
/// fallback for a `java.net.URI` that arrived fully constructed from real
/// bytecode and never cached its text.
///
/// An EMPTY authority is `None`, not `Some("")`: `new URI("file:///tmp/x")`
/// has a zero-length authority between the `//` and the path, and both the JDK
/// and this VM must answer `null` for it.
pub fn uri_authority_component(ctx: &dyn NativeContext, uri: ObjectRef) -> Option<String> {
    let raw = uri_raw_string(ctx, uri);
    if !raw.is_empty() {
        return uri_split(&raw).1.filter(|a| !a.is_empty());
    }
    match ctx.get_field_by_name(uri, "authority") {
        Value::Object(Some(s)) => ctx.read_string(s).filter(|a| !a.is_empty()),
        _ => None,
    }
}

/// The connection-relevant components of a `java.net.URI`, read WITHOUT a
/// hand-numbered slot index.
///
/// `native-builtins/src/http2.rs` and `native-builtins/src/servlet.rs` each
/// carried an identical private copy of the fabricated
/// `scheme=0, host=1, port=2, path=3, query=4` model and read their URI
/// receivers through it. Two copies of a layout is how the
/// `real_protected_stub` allow-lists drifted, and on a real `java.net.URI`
/// every one of those indices names a different field (`fragment`,
/// `authority`, `userInfo`, `host`), so the readers were reporting the wrong
/// component with no fault and no log line — an HTTP client dialling the
/// fragment as its host.
///
/// One helper, parsing the same raw string the registered accessors parse, so
/// there is no second model left to drift.
pub struct UriComponents {
    pub scheme: Option<String>,
    pub host: Option<String>,
    /// `-1` when the URI carries no explicit port, matching `URI.getPort()`.
    pub port: i32,
    pub path: String,
    pub query: Option<String>,
}

pub fn uri_components(ctx: &dyn NativeContext, uri: ObjectRef) -> UriComponents {
    let raw = uri_raw_string(ctx, uri);
    if !raw.is_empty() {
        let (scheme, authority, path, query, _) = uri_split(&raw);
        let (host, port) = match authority.as_deref() {
            Some(a) if !a.is_empty() => {
                let (_, h, p) = uri_parse_authority(a);
                (h, p)
            }
            _ => (None, -1),
        };
        return UriComponents {
            scheme,
            host,
            port,
            path,
            query,
        };
    }
    // No cached text: a real `java.net.URI` built by real bytecode. Its own
    // fields are the answer, by name.
    let str_field = |name: &str| match ctx.get_field_by_name(uri, name) {
        Value::Object(Some(s)) => ctx.read_string(s).filter(|v| !v.is_empty()),
        _ => None,
    };
    UriComponents {
        scheme: str_field("scheme"),
        host: str_field("host"),
        port: match ctx.get_field_by_name(uri, "port") {
            Value::Int(p) => p,
            _ => -1,
        },
        path: str_field("path").unwrap_or_default(),
        query: str_field("query"),
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
pub fn uri_equals(ctx: &dyn NativeContext, a: ObjectRef, b: ObjectRef) -> bool {
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
pub fn uri_hash_code(ctx: &dyn NativeContext, uri: ObjectRef) -> i32 {
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

/// `java.net.URI.compareTo(null)`.
///
/// The JDK has no null check here — `compareTo`'s first statement is
/// `compareIgnoringCase(this.scheme, that.scheme)`, so the argument is
/// dereferenced and the helpful NPE names the field it was reading. Measured
/// on HotSpot 25.0.4+7:
///
/// ```text
/// java.lang.NullPointerException: Cannot read field "scheme" because "that" is null
/// ```
///
/// This registration cannot get there by dereferencing (it never touches a
/// field), so the message is a constant — but an `IllegalArgumentException`,
/// which is what stood here, is a different type on the wire and a
/// `catch (NullPointerException)` walks straight past it.
pub fn uri_compare_to_null() -> cratonvm_types::error::MethodCallFailed {
    npe("Cannot read field \"scheme\" because \"that\" is null")
}

/// The `IllegalArgumentException` `java.net.URI.create(String)` throws for a
/// parse failure: the `URISyntaxException`'s own `getMessage()` as the
/// message, and that exception as the CAUSE.
///
/// MEASURED on HotSpot 25.0.4+7:
///
/// ```text
///   URI.create("http://h/%zz")
///     java.lang.IllegalArgumentException: Malformed escape pair at index 9: http://h/%zz
///     cause java.net.URISyntaxException: Malformed escape pair at index 9: http://h/%zz
/// ```
///
/// The JDK gets both for free — `catch (URISyntaxException x) { throw new
/// IllegalArgumentException(x.getMessage(), x); }` — and this reproduces that
/// literally rather than re-deriving the text, so the two doors cannot drift.
pub fn uri_create_iae(
    ctx: &mut dyn NativeContext,
    input: &str,
    fail: &crate::UriParseFail,
) -> cratonvm_types::error::MethodCallFailed {
    // The text the JDK's own `getMessage()` would produce, kept as the
    // fallback for the (allocation-failure only) paths below.
    let trimmed = crate::uri_exception_input(input);
    let text = match fail.index {
        Some(i) => format!("{} at index {i}: {trimmed}", fail.reason),
        None => format!("{}: {trimmed}", fail.reason),
    };
    let Some(MethodCallFailed::ExceptionThrown(cause)) =
        crate::uri_syntax_exception_pub(ctx, input, fail)
    else {
        return iae(text);
    };
    // `getMessage()` runs bytecode and the constructor below allocates, so the
    // cause has to survive both.
    let pin = ctx.pin_native_root(cause);
    let cause = ctx.read_native_pin(pin, cause);
    let msg = match ctx.invoke_virtual(cause, "getMessage", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(m)))) => m,
        _ => ctx.create_string(&text),
    };
    let cause = ctx.read_native_pin(pin, cause);
    ctx.unpin_native_roots(pin);
    cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "java/lang/IllegalArgumentException",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        &[Value::Object(Some(msg)), Value::Object(Some(cause))],
        || iae(text),
    )
}
fn cmp_order(o: std::cmp::Ordering) -> i32 {
    match o {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

pub fn cmp_i32(a: i32, b: i32) -> i32 {
    cmp_order(a.cmp(&b))
}

pub fn cmp_str(a: &str, b: &str) -> i32 {
    cmp_order(a.cmp(b))
}

pub fn cmp_opt_str(a: Option<&str>, b: Option<&str>) -> i32 {
    match (a, b) {
        (None, None) => 0,
        (None, Some(_)) => -1,
        (Some(_), None) => 1,
        (Some(x), Some(y)) => cmp_str(x, y),
    }
}

pub fn cmp_opt_str_ci(a: Option<&str>, b: Option<&str>) -> i32 {
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
pub fn uri_compare(ctx: &dyn NativeContext, a: ObjectRef, b: ObjectRef) -> i32 {
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

pub fn opt_str_eq_ignore_case(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => false,
    }
}

/// `java.lang.String.hashCode()` over UTF-16 code units.
///
/// Not `str::bytes()`: `java.net.URI` hashes CHARS, and for any component
/// carrying a non-ASCII character (`http://h/\u{e9}`) a byte fold and a
/// code-unit fold give different numbers.
pub fn java_string_hash(s: &str) -> i32 {
    s.encode_utf16()
        .fold(0i32, |acc, u| acc.wrapping_mul(31).wrapping_add(u as i32))
}

/// `java.net.URI.normalizedHash(int, String)` — the arm taken for a component
/// that contains a `%`, so that two URIs differing only in the CASE of an
/// escape triplet (`%c3%a9` vs `%C3%A9`) hash alike, as `URI.equals` requires.
pub fn uri_normalized_hash(h: i32, s: &str) -> i32 {
    let u: Vec<u16> = s.encode_utf16().collect();
    let mut inner: i32 = 0;
    let mut i = 0usize;
    while i < u.len() {
        let c = u[i];
        inner = inner.wrapping_mul(31).wrapping_add(c as i32);
        if c == u16::from(b'%') {
            // The next TWO units, upper-cased, exactly as the JDK does — and
            // unguarded there too, because `%` only survives the parser as the
            // first unit of a well-formed triplet.
            for k in (i + 1)..(i + 3) {
                let d = u.get(k).copied().unwrap_or(0);
                let up = if (b'a' as u16..=b'z' as u16).contains(&d) {
                    d - 32
                } else {
                    d
                };
                inner = inner.wrapping_mul(31).wrapping_add(up as i32);
            }
            i += 2;
        }
        i += 1;
    }
    h.wrapping_mul(127).wrapping_add(inner)
}

/// `java.net.URI.hash(int, String)` — the case-SENSITIVE component step.
///
/// This is `h * 127 + s.hashCode()`, a fresh string hash mixed into the
/// accumulator, NOT a continuation of one 31-based fold across the whole URI.
/// The difference is not cosmetic: it was worth **31 of `L6UriSweep`'s 40
/// differing rows**, every `hashCode` row the probe asks.
pub fn hash_str(h: i32, s: Option<&str>) -> i32 {
    match s {
        None => h,
        Some(s) if !s.contains('%') => h.wrapping_mul(127).wrapping_add(java_string_hash(s)),
        Some(s) => uri_normalized_hash(h, s),
    }
}

/// `java.net.URI.hashIgnoringCase(int, String)` — used for `scheme` and
/// `host`, the two components `URI.equals` compares case-insensitively. Unlike
/// [`hash_str`] this one really does continue the accumulator's 31-fold.
pub fn hash_str_ignore_case(h: i32, s: Option<&str>) -> i32 {
    match s {
        None => h,
        Some(s) => s.encode_utf16().fold(h, |acc, u| {
            let low = if (b'A' as u16..=b'Z' as u16).contains(&u) {
                u + 32
            } else {
                u
            };
            acc.wrapping_mul(31).wrapping_add(low as i32)
        }),
    }
}

/// True when `p` looks like a Windows absolute path with a drive letter
/// (`C:/…` or `C:\…`), i.e. a `file:` URL path whose leading `/` has already
/// been trimmed. Used to decide whether a leading slash is the POSIX root
/// (keep it) or the `file:`-URL artefact before a drive letter (drop it).
pub fn is_windows_drive_path(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
}

/// Percent-decode a URI component the way `java.net.URI` getters do: each
/// `%XX` triplet is one byte, the byte sequence is interpreted as UTF-8, and
/// every other character (INCLUDING `+`, which URI leaves literal — unlike
/// `application/x-www-form-urlencoded`) is copied verbatim. A malformed `%`
/// escape (missing/non-hex digits) is copied through unchanged.
pub fn uri_percent_decode(input: &str) -> String {
    let units: Vec<u16> = input.encode_utf16().collect();
    String::from_utf16_lossy(&uri_percent_decode_units(&units))
}

/// [`uri_percent_decode`] in code units — the implementation of both.
///
/// Consecutive `%XX` escapes are collected into a RUN and UTF-8 decoded
/// together, because a non-ASCII character is encoded as several escapes and
/// only decodes correctly as a group. Units outside an escape run are copied
/// through verbatim, which is what makes an unpaired surrogate survive: it is
/// never routed through a `str` at all.
pub fn uri_percent_decode_units(input: &[u16]) -> Vec<u16> {
    if !input.contains(&u16::from(b'%')) {
        return input.to_vec();
    }
    let open = u16::from(b'[');
    let close = u16::from(b']');
    let mut out: Vec<u16> = Vec::with_capacity(input.len());
    let mut i = 0;
    // INSIDE `[...]` A `%` IS LITERAL.
    //
    // `java.net.URI.decode(String)` is `decode(s, true)` — its parameter is
    // named `ignorePercentInBrackets` — and every decoded accessor reaches it
    // through that one entry point. The exemption is RFC 6874's: the `%` of a
    // zone identifier `%25eth0` is part of the literal address, and decoding it
    // destroys the only thing separating a zone id from a percent-escape.
    //
    // MEASURED against HotSpot 25.0.4+7 by
    // `apps/probes/UriRecompositionSweep.java`:
    //
    //   URI.create("http://[fe80::1%25eth0]/a").getAuthority()
    //     HotSpot   [fe80::1%25eth0]
    //     was       [fe80::1%eth0]
    //
    // and `getSchemeSpecificPart()` beside it, through this same helper.
    let mut in_brackets = false;
    while i < input.len() {
        if input[i] == open {
            in_brackets = true;
        } else if input[i] == close {
            in_brackets = false;
        }
        if input[i] == u16::from(b'%') && !in_brackets {
            let mut bytes: Vec<u8> = Vec::new();
            let mut j = i;
            while j + 2 < input.len() && input[j] == u16::from(b'%') {
                match (u_hex(input[j + 1]), u_hex(input[j + 2])) {
                    (Some(h), Some(l)) => {
                        bytes.push((h << 4) | l);
                        j += 3;
                    }
                    _ => break,
                }
            }
            if !bytes.is_empty() {
                out.extend(String::from_utf8_lossy(&bytes).encode_utf16());
                i = j;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

/// The raw query of a URI, or `None` when it has none.
///
/// A `?` that appears AFTER the `#` belongs to the fragment, not to a query —
/// the same precedence the text splitter applies.
///
/// An OPAQUE URI has NO query at all: its whole scheme-specific part is one
/// undivided string in which `?` is an ordinary character, and only the
/// fragment is split off it (`java.net.URI`'s `parse`/`parseHierarchical`
/// split — `Parser.parse` only calls `parseHierarchical` when the SSP begins
/// with `/`). `mailto:user@example.com?subject=hello` therefore has
/// `getRawQuery() == null` and a scheme-specific part of
/// `user@example.com?subject=hello`, which is exactly what Spring's
/// `WebClientUtils.getRequestDescription` leans on — its
/// `rawUserInfo == null && rawQuery == null && rawFragment == null` test is
/// commented "also handles Opaque URI, which has only schemeSpecificPart".
/// Answering `subject=hello` here sent it down the hierarchical rebuild, which
/// has no path or host to append and produced `GET mailto:`
/// (`WebClientUtilsTests.opaqueUriUnchanged`; `probes/OpaqueUriProbe.java`
/// rows O01/O04/O09/O11/C01/C03).
///
/// Opacity is asked of [`uri_select_raw_path_units`] rather than spelled a
/// second time here: "the path is null" IS the JDK's definition of an opaque
/// URI, so the two can never drift apart. The `getQuery`/`getRawQuery`
/// registrations already CLAIMED to handle this ("Opaque URIs keep a literal
/// `?` inside the scheme-specific part"); they lost it when they moved off
/// `uri_split` onto this splitter.
pub fn uri_query_units(raw: &[u16]) -> Option<Vec<u16>> {
    uri_select_raw_path_units(raw)?;
    let hash = u_find(raw, b'#');
    let q = u_find(raw, b'?').filter(|&qi| hash.is_none_or(|h| qi < h))?;
    let rest = &raw[q + 1..];
    let end = u_find(rest, b'#').unwrap_or(rest.len());
    Some(rest[..end].to_vec())
}

/// The raw fragment of a URI, or `None` when it has none.
pub fn uri_fragment_units(raw: &[u16]) -> Option<Vec<u16>> {
    u_find(raw, b'#').map(|i| raw[i + 1..].to_vec())
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
pub fn uri_select_raw_path(raw: &str) -> Option<String> {
    let units: Vec<u16> = raw.encode_utf16().collect();
    uri_select_raw_path_units(&units).map(|u| String::from_utf16_lossy(&u))
}

/// [`uri_select_raw_path`] in code units — the implementation of both. The
/// doc comment above describes the rule; this is where it lives.
pub fn uri_select_raw_path_units(raw: &[u16]) -> Option<Vec<u16>> {
    let (is_absolute, ssp) = match u_find(raw, b':') {
        Some(i) => {
            let scheme = &raw[..i];
            let scheme_ok = !scheme.is_empty() && u_find_any(scheme, b"/?#").is_none();
            if scheme_ok {
                (true, &raw[i + 1..])
            } else {
                (false, raw)
            }
        }
        None => (false, raw),
    };
    if is_absolute && !u_starts_with(ssp, "/") {
        return None; // opaque URI -> null path
    }
    let after_auth = if u_starts_with(ssp, "//") {
        let rest = &ssp[2..];
        match u_find_any(rest, b"/?#") {
            Some(p) => &rest[p..],
            None => &[][..],
        }
    } else {
        ssp
    };
    let end = u_find_any(after_auth, b"?#").unwrap_or(after_auth.len());
    Some(after_auth[..end].to_vec())
}

/// Is this URI text OPAQUE — absolute, with a scheme-specific part that does
/// not begin with `/`?
///
/// Defined as "the path is null", which is how `java.net.URI` itself draws the
/// line, so this can never disagree with [`uri_select_raw_path_units`].
/// The empty string is a relative reference, not an opaque URI.
pub fn uri_text_is_opaque(raw: &str) -> bool {
    let units: Vec<u16> = raw.encode_utf16().collect();
    uri_select_raw_path_units(&units).is_none()
}

/// Byte index of the scheme-terminating `:`, or `None` for a relative
/// reference. Mirrors the real JDK parser (`uri_scheme_name_fail_index` in
/// lib.rs): scan for the first stop char among `:/?#`; only a `:` counts,
/// and the text before it must be a valid scheme name (ALPHA start,
/// alphanum/`+`/`-`/`.` body). Without this rule a colon inside a relative
/// path ("/redirect:account", Spring's view-name redirect tests) was taken
/// as a scheme delimiter, corrupting scheme/ssp/path derivation.
pub fn uri_scheme_colon(raw: &str) -> Option<usize> {
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
pub fn uri_raw_scheme_specific_part(raw: &str) -> String {
    let units: Vec<u16> = raw.encode_utf16().collect();
    String::from_utf16_lossy(&uri_raw_scheme_specific_part_units(&units))
}

/// [`uri_raw_scheme_specific_part`] in code units — the implementation of both.
///
/// `uri_scheme_colon` is asked on the TEXT and its answer reused as a unit
/// index. That is exact and not a coincidence: it returns the offset of an
/// ASCII `:` that must precede any `/?#`, so every unit before it is ASCII and
/// the two indexings agree by construction. Anything that made a non-ASCII
/// character legal before the scheme colon would break this, which is why it is
/// said out loud rather than left to be noticed.
pub fn uri_raw_scheme_specific_part_units(raw: &[u16]) -> Vec<u16> {
    let text = String::from_utf16_lossy(raw);
    let ssp = match uri_scheme_colon(&text) {
        Some(colon) => &raw[colon + 1..],
        None => raw,
    };
    let end = u_find(ssp, b'#').unwrap_or(ssp.len());
    ssp[..end].to_vec()
}

/// RFC 3986 §5.3 path-merge: combine a base hierarchical path with a
/// relative reference path.
pub fn uri_merge_paths(base_path: &str, ref_path: &str) -> String {
    // `java.net.URI.resolvePath`, which is simply "everything up to and
    // including the base's last `/`, then the child" — with NO special case for
    // an authority-bearing base whose path is empty. That case used to
    // fabricate a leading `/` here, so `URI.create("https://h").resolve("")`
    // answered `https://h/` where HotSpot answers `https://h`
    // (`probes/OpaqueUriProbe.java` S17: the JDK's `i >= 0` guard simply
    // declines to prepend anything when there is no `/` to cut at).
    match base_path.rfind('/') {
        Some(i) => {
            let mut s = base_path[..=i].to_string();
            s.push_str(ref_path);
            s
        }
        None => ref_path.to_string(),
    }
}

/// RFC 3986 §5.2.4 — remove `.` and `..` segments from a path.
pub fn uri_remove_dot_segments(path: &str) -> Result<String, MethodCallFailed> {
    let units: Vec<u16> = path.encode_utf16().collect();
    Ok(String::from_utf16_lossy(&uri_remove_dot_segments_units(
        &units,
    )?))
}

/// [`uri_remove_dot_segments`] in code units — the implementation of both.
///
/// Segments split on ASCII `/`, and the only two segments with meaning, `.` and
/// `..`, are ASCII. Every other segment is copied as an opaque run of units, so
/// text this function has no opinion about passes through untouched.
///
/// That matters here more than anywhere else in the URI family: `relativize` is
/// the one operation that REBUILDS a path from segments instead of selecting a
/// range of one, and rebuilding is exactly where a lossy spelling puts the
/// substitution back after the accessors have avoided it (G75-1 N3).
pub fn uri_remove_dot_segments_units(path: &[u16]) -> Result<Vec<u16>, MethodCallFailed> {
    let slash = u16::from(b'/');
    let dot = u16::from(b'.');
    let is_dot = |s: &[u16]| s == [dot];
    let is_dotdot = |s: &[u16]| s == [dot, dot];

    let absolute = path.first() == Some(&slash);
    let segs: Vec<&[u16]> = path.split(|&c| c == slash).collect();
    // A path whose final segment is "." or ".." resolves to a directory, so the
    // output ends with '/' — but ONLY when that segment was CONSUMED.
    //
    // The `..` that this function deliberately KEEPS (see the DEVIATION note
    // below) is an ordinary segment in the result, and the JDK's `join` does
    // not put a separator after it. MEASURED against HotSpot 25.0.4+7 by
    // `apps/probes/UriRecompositionSweep.java`:
    //
    //   URI.create("http://host/a/b").resolve("../..")   http://host/..   was http://host/../
    //   URI.create("..").normalize()                     ..               was ../
    //   URI.create(".").normalize()                      <empty>          was /
    //
    // The last row is the second half of the rule: a relative path whose
    // segments all vanish is the EMPTY string, not "/". `trailing_owed` is
    // therefore resolved against the finished `out` rather than against the
    // input alone.
    let final_dot_segment = segs.last().is_some_and(|l| is_dot(l) || is_dotdot(l));
    let ended_with_slash = path.last() == Some(&slash);
    let mut out: Vec<&[u16]> = Vec::new();
    for seg in &segs {
        if seg.is_empty() || is_dot(seg) {
            continue;
        }
        if is_dotdot(seg) {
            // `java.net.URI.normalize` does NOT clamp at the root, and RFC 3986's
            // `remove_dot_segments` does — this used to do the latter. A `..`
            // with nothing to pop, or with another `..` already on top, is KEPT.
            // MEASURED on HotSpot 25 (`probes/OpaqueUriProbe.java`):
            //
            //   /a/../../x     -> /../x        (N02)      not  /x
            //   a/../../b      -> ../b         (N04)      not  b
            //   a/b/../../../c -> ../c         (N10)      not  c
            //
            // The JDK's own `removeDots` says so in a comment — "DEVIATION:
            // RFC2396 says .. can be removed even if it is at the start" — and
            // then declines to. `resolve` inherits the rule through this
            // function, which is why `https://h/a/b` resolve `../../x` is
            // `https://h/../x` (S27).
            match out.last() {
                Some(prev) if !is_dotdot(prev) => {
                    out.pop();
                }
                _ => out.push(seg),
            }
            continue;
        }
        out.push(seg);
    }
    let mut result: Vec<u16> = Vec::new();
    if absolute {
        result.push(slash);
    }
    for (i, seg) in out.iter().enumerate() {
        if i > 0 {
            result.push(slash);
        }
        result.extend_from_slice(seg);
    }
    // The final `.`/`..` was consumed unless it is still standing at the end of
    // `out` — which only a KEPT `..` can be.
    let kept_final_dotdot = final_dot_segment && out.last().is_some_and(|l| is_dotdot(l));
    let trailing_slash = ended_with_slash || (final_dot_segment && !kept_final_dotdot);
    if trailing_slash && !out.is_empty() && result.last() != Some(&slash) {
        result.push(slash);
    }
    Ok(result)
}

/// Split a URI string into (scheme, authority, path, query, fragment).
/// `authority` is `None` when the URI has no `//` authority component.
pub fn uri_split(
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
        if end == 0 {
            // AN EMPTY AUTHORITY IS NOT AN AUTHORITY.
            //
            // `java.net.URI.parseHierarchical` scans the authority region and,
            // when it is empty, leaves `authority` NULL and lets the rest be
            // the path — it does not record a present-but-empty authority.
            // This returned `Some("")` instead, and `uri_recompose` emits `//`
            // for any `Some`, so every path that RECOMPOSES kept a `///` the
            // JDK drops:
            //
            //   URI.create("file:///C:/tmp/f.txt").resolve("x")
            //     HotSpot   file:/C:/tmp/x
            //     was       file:///C:/tmp/x
            //
            // The accessors were already right — both VMs answer
            // `getAuthority() == null` for `file:///a`, and `toString()` of the
            // parsed URI is the input text either way — so this was visible
            // ONLY through an operation that rebuilds the string. That is why
            // `uri-resolve-folded-a-reference-into-the-host-and-the-locale-display-name-gap-20260826-RETIRED-20260920.md` §4
            // saw a single row and deferred it as not worth the blast radius:
            // its probe asked one. `UriRecompositionSweep` asks 1258 and finds
            // **26**, every one of them a `resolve` off an empty-authority base.
            (None, after.to_string())
        } else {
            (Some(after[..end].to_string()), after[end..].to_string())
        }
    } else {
        (None, without_query.to_string())
    };
    (scheme, authority, path, query, fragment)
}

/// Split a URI authority (`[userinfo "@"] host [":" port]`, RFC 3986 §3.2) into
/// `(userInfo, host, port)`. Host of an IPv6 literal keeps its brackets
/// (`[::1]`), matching `java.net.URI.getHost()`.
///
/// **The three components stand or fall TOGETHER.** `java.net.URI` parses an
/// authority twice: first as *server-based* (`parseServer`), and — when that
/// fails and `requireServerAuthority` is false, which is the case for every
/// `new URI(String)` — it **undoes the whole attempt** and keeps the authority
/// as *registry-based*:
///
/// ```text
/// } catch (URISyntaxException x) {
///     // Undo results of failed parse
///     userInfo = null;
///     host = null;
///     port = -1;
/// ```
/// (`java.base/java/net/URI.java`, `Parser.parseAuthority`, JDK 25.0.3+9.)
///
/// So `new URI("http://h:-5/p")` is a perfectly legal URI whose
/// `getAuthority()` is `h:-5` and whose `getHost()`, `getUserInfo()` and
/// `getPort()` are `null`, `null` and `-1`. This function used to end in
/// `p.parse::<i32>().ok()`, which took Rust's leading `+`/`-` and answered
/// `-5`/`80` where HotSpot answers `-1`, and kept a non-null host besides —
/// `RJdkBridge1`'s check 197. Clamping only the port would have satisfied
/// `RJdkBridge1.java:1181` and failed `:1183`; the demotion is the contract.
///
/// Returning `(None, None, -1)` here is also what the callers already expect:
/// [`uri_equals`], [`uri_hash_code`] and `uri_compare` all branch on
/// `host.is_some()` to pick the JDK's server-vs-registry comparison, so a
/// demoted authority routes them onto the raw-string branch by itself.
pub fn uri_parse_authority(authority: &str) -> (Option<String>, Option<String>, i32) {
    uri_parse_server_authority(authority).unwrap_or((None, None, -1))
}

/// Transcription of `java.net.URI$Parser.parseServer` (JDK 25.0.3+9) as a pure
/// function over the authority text. `None` is the JDK's "server-based parse
/// failed", which `parseAuthority` turns into a registry-based authority.
///
/// The JDK's `checkChars(p, q, L_USERINFO, …, "user info")` is deliberately NOT
/// transcribed, and that is not a gap: `L_REG_NAME` and `L_USERINFO` differ by
/// exactly one character, `@`, and an `@` inside the user-info span can only
/// arise when a *later* `@` was chosen as the delimiter — which never happens,
/// because the JDK takes the FIRST `@` (`scan(p, n, "/?#", "@")` stops at it),
/// and the text after it then fails `parseHostname` anyway. MEASURED:
/// `new URI("http://a@b@c/p").getUserInfo()` is `null` on HotSpot. Every other
/// character that is legal in a user info but not in an authority is already
/// refused upstream by the whole-authority character check.
pub fn uri_parse_server_authority(a: &str) -> Option<(Option<String>, Option<String>, i32)> {
    let b = a.as_bytes();
    let n = b.len();
    let mut p = 0usize;

    // userinfo — `q = scan(p, n, "/?#", "@")`, i.e. the FIRST '@', and never
    // across a '/', '?' or '#'.
    let mut user_info: Option<String> = None;
    {
        let mut q = p;
        let mut at = None;
        while q < n {
            match b[q] {
                b'/' | b'?' | b'#' => break,
                b'@' => {
                    at = Some(q);
                    break;
                }
                _ => q += 1,
            }
        }
        if let Some(q) = at {
            user_info = Some(a[p..q].to_string());
            p = q + 1; // skip '@'
        }
    }

    // hostname, IPv4 address, or IPv6 address
    let host;
    if p < n && b[p] == b'[' {
        // DEVIATION from RFC2396, per RFC2732: an IPv6 literal. The body's own
        // grammar (`parseIPv6Reference`, scope ids) is policed by the URI
        // constructor before an object exists, so it is not re-checked here;
        // what matters for the accessors is where the host ends.
        let start = p;
        p += 1;
        let mut q = p;
        let mut close = None;
        while q < n {
            match b[q] {
                b'/' | b'?' | b'#' => break,
                b']' => {
                    close = Some(q);
                    break;
                }
                _ => q += 1,
            }
        }
        match close {
            Some(q) if q > p => {
                host = Some(a[start..=q].to_string());
                p = q + 1;
            }
            // failExpecting("closing bracket for IPv6 address")
            _ => return None,
        }
    } else {
        match uri_parse_ipv4_address(b, p, n) {
            Some(q) if q > p => {
                host = Some(a[p..q].to_string());
                p = q;
            }
            _ => {
                let (h, q) = uri_parse_hostname(a, p, n)?;
                host = Some(h);
                p = q;
            }
        }
    }

    // port
    let mut port = -1i32;
    if p < n && b[p] == b':' {
        p += 1;
        // `q = scan(p, n, "/")` — an authority never contains a '/', so the
        // port runs to the end of the authority. That is why `http://h:80:90/p`
        // has port text "80:90" and demotes, rather than splitting on the LAST
        // colon and answering 90.
        let q = n;
        if q > p {
            // checkChars(p, q, L_DIGIT, H_DIGIT, "port number") — DIGITS ONLY.
            // No sign, no space, no trailing junk.
            if !b[p..q].iter().all(u8::is_ascii_digit) {
                return None;
            }
            // Integer.parseInt(input, p, q, 10); a NumberFormatException here
            // is `fail("Malformed port number")`, which demotes just the same.
            // Leading zeros are fine and are not part of the value:
            // MEASURED `http://h:00000000080/p` → 80, `http://h:007/p` → 7.
            port = a[p..q].parse::<i32>().ok()?;
            p = q;
        }
    } else if p < n {
        // `else if (p < n && skipParseException) return p;` — the caller then
        // sees `q < n` and demotes.
        return None;
    }

    if p < n {
        // failExpecting("port number", p)
        return None;
    }
    Some((user_info, host, port))
}

/// Transcription of `java.net.URI$Parser.parseHostname` (JDK 25.0.3+9).
/// Returns `(host, end)` or `None` when the hostname parse fails — which is
/// the JDK's `fail(...)`, and also its `skipParseException` early return, both
/// of which end in the same registry-based demotion.
///
/// The grammar is `domainlabel = alphanum [ *( alphanum | "-" ) alphanum ]`,
/// labels joined by `.`, and one extra rule that is NOT in RFC 2396: when
/// there is more than one label, the LAST one must START with a letter.
/// MEASURED: `http://a.9b/p` has a null host and `http://a.b9/p` does not,
/// while the single-label `http://9h/p` and `http://12/p` are both fine
/// because the check is `l > start`.
pub fn uri_parse_hostname(a: &str, start: usize, n: usize) -> Option<(String, usize)> {
    let b = a.as_bytes();
    let mut p = start;
    let mut l: Option<usize> = None; // start of last parsed label
    loop {
        let q = {
            let mut q = p;
            while q < n && b[q].is_ascii_alphanumeric() {
                q += 1;
            }
            q
        };
        if q <= p {
            break;
        }
        l = Some(p);
        p = q;
        let q = {
            let mut q = p;
            while q < n && (b[q].is_ascii_alphanumeric() || b[q] == b'-') {
                q += 1;
            }
            q
        };
        if q > p {
            if b[q - 1] == b'-' {
                // fail("Illegal character in hostname", q - 1)
                return None;
            }
            p = q;
        }
        if !(p < n && b[p] == b'.') {
            break;
        }
        p += 1;
        if p >= n {
            break;
        }
    }
    if p < n && b[p] != b':' {
        return None;
    }
    let l = l?; // if (l < 0) failExpecting("hostname", start)
    if l > start && !b[l].is_ascii_alphabetic() {
        return None;
    }
    Some((a[start..p].to_string(), p))
}

/// Transcription of `java.net.URI$Parser.parseIPv4Address` plus the non-strict
/// arm of `scanIPv4Address` (JDK 25.0.3+9). `Some(end)` when the text at
/// `start` begins with a legal dotted quad that is followed by nothing or by a
/// `:`; `None` otherwise, which sends the caller to `parseHostname`.
pub fn uri_parse_ipv4_address(b: &[u8], start: usize, n: usize) -> Option<usize> {
    let p = uri_scan_ipv4_address(b, start, n)?;
    if p > start && p < n && b[p] != b':' {
        return None;
    }
    Some(p)
}

pub fn uri_scan_ipv4_address(b: &[u8], start: usize, n: usize) -> Option<usize> {
    let mut p = start;
    // m = scan(p, n, L_DIGIT | L_DOT, …) — the address may only be looked for
    // inside the leading run of digits and dots.
    let mut m = p;
    while m < n && (b[m].is_ascii_digit() || b[m] == b'.') {
        m += 1;
    }
    if m <= p {
        return None;
    }
    // The JDK's unrolled four-byte loop: `scanByte`, `.`, `scanByte`, `.`,
    // `scanByte`, `.`, `scanByte`, and then the whole digit-and-dot run must be
    // consumed. Any short break is a failure (the non-strict arm returns -1).
    for i in 0..4 {
        if i > 0 {
            if !(p < m && b[p] == b'.') {
                return None;
            }
            p += 1;
        }
        let q = uri_scan_byte(b, p, m);
        if q <= p {
            return None;
        }
        p = q;
    }
    if p < m {
        return None;
    }
    Some(p)
}

/// `java.net.URI$Parser.scanByte` — a run of decimal digits whose value fits in
/// a byte. Leading zeros do not count towards the three-digit limit, which is
/// why `http://01.2.3.4/p` has host `01.2.3.4` on HotSpot (MEASURED) while
/// `http://256.1.1.1/p` has none.
pub fn uri_scan_byte(b: &[u8], start: usize, n: usize) -> usize {
    let p = start;
    let mut q = p;
    while q < n && b[q].is_ascii_digit() {
        q += 1;
    }
    if q <= p {
        return q;
    }
    let mut i = p;
    while i < q && b[i] == b'0' {
        i += 1;
    }
    let significant = q - i;
    if significant < 3 {
        return q; // definitely < 255
    }
    if significant > 3 {
        return p; // definitely > 255
    }
    // Exactly three significant digits: at most 999, so no overflow is possible
    // and the JDK's `Integer.parseInt` cannot throw here.
    let v: u32 = std::str::from_utf8(&b[i..q])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(u32::MAX);
    if v > 255 {
        return p;
    }
    q
}

/// Match `java.net.URI`'s server-based host acceptance for the cases keycloak's
/// validators care about: a dotted-decimal that LOOKS like IPv4 must be a valid
/// IPv4 (4 octets, each 0-255) or the host is rejected (null). IPv6 literals
/// (`[...]`) and reg-names (anything not pure digits+dots) are accepted.
pub fn uri_host_is_valid(host: &str) -> bool {
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
pub fn uri_resolve_ref(base: &str, reference: &str) -> Result<String, MethodCallFailed> {
    // `URI.resolve`'s very first act: "if (child.isOpaque() || base.isOpaque())
    // return child" — verbatim, and BEFORE the empty-reference shortcut, since
    // RFC 3986 §5 reference resolution is defined only over hierarchical URIs.
    // An opaque base has no path to merge against, so nothing of it survives
    // into the result. Measured: `URI.create("mailto:a@b").resolve("c@d")` is
    // `c@d` on HotSpot and was `mailto:c@d` here
    // (`probes/OpaqueUriProbe.java` row R01).
    //
    // Note the JDK returns the child UNNORMALIZED in this arm — it is the
    // argument object itself, not a rebuild — so this returns the reference
    // text verbatim rather than routing it through `uri_remove_dot_segments`.
    if uri_text_is_opaque(reference) || uri_text_is_opaque(base) {
        return Ok(reference.to_string());
    }
    let (r_scheme, r_auth, r_path, r_query, r_frag) = uri_split(reference);
    // 5.2 (3) — a reference that carries its own scheme IS the result. The JDK
    // returns the argument OBJECT here, so it is NOT normalized: measured,
    // `https://h/a/b` resolve `https://x/p/../q` is `https://x/p/../q`, dot
    // segments and all (`probes/OpaqueUriProbe.java` S11/S12, and R15 — the
    // row this arm used to get wrong by rebuilding through
    // `uri_remove_dot_segments`).
    if r_scheme.is_some() {
        return Ok(reference.to_string());
    }
    let (b_scheme, b_auth, b_path, b_query, _b_frag) = uri_split(base);
    // 5.2 (2) — "reference to the current document": a LONE fragment, and only
    // that. The base's path AND query both survive it, which is what separates
    // it from every other empty-path reference. `?q=2` does not qualify (its
    // query is non-null) and neither does the empty string (no fragment), and
    // both of those take the 6a directory step below instead — S02 against
    // S01/S03.
    if r_auth.is_none() && r_path.is_empty() && r_frag.is_some() && r_query.is_none() {
        return Ok(uri_recompose(
            &b_scheme, &b_auth, &b_path, &b_query, &r_frag,
        ));
    }
    // Every remaining arm takes the CHILD's query and fragment, never the
    // base's — `ru.query = child.query` sits above the authority branch in the
    // JDK. That is why `https://h/a/b?q=1` resolve `<empty>` loses `q=1`.
    let (t_auth, t_path);
    if r_auth.is_some() {
        // 5.2 (4) — the reference's authority wins and its path is taken
        // VERBATIM. Measured: `//other/p/../q` keeps its dot segments (S10).
        t_auth = r_auth;
        t_path = Ok(r_path.clone());
    } else {
        t_auth = b_auth.clone();
        if r_path.starts_with('/') {
            // 5.2 (5) — an absolute reference path is verbatim too (S25/S26).
            t_path = Ok(r_path.clone());
        } else {
            // 5.2 (6) — merge against the base's DIRECTORY, then normalize.
            // An EMPTY reference path lands here as well, which is the whole
            // of R13: `https://h/a/b?q=1` resolve `<empty>` is `https://h/a/`,
            // not the base.
            let merged = uri_merge_paths(&b_path, &r_path);
            // ROOT the merged path when the base is absolute AND carries an
            // authority. `uri_merge_paths` is `java.net.URI.resolvePath`, whose
            // `i >= 0` guard prepends nothing when the base path has no `/` --
            // correct for the base path itself, but it leaves a non-empty
            // merged path unrooted, and recomposition then CONCATENATES it onto
            // the authority. MEASURED: `URI.create("http://host").resolve("x")`
            // answered `http://hostx` against HotSpot's `http://host/x`
            // (`probes/UriLocaleSweep.java`), i.e. a relative reference silently
            // became part of the HOST.
            //
            // The empty-child case is deliberately untouched and is why this is
            // conditioned on `!merged.is_empty()` rather than applied in
            // `uri_merge_paths`: `URI.create("https://h").resolve("")` must stay
            // `https://h`, NOT `https://h/`, which `OpaqueUriProbe` row S17
            // measured and the merge helper's own comment records.
            let merged = if b_scheme.is_some()
                && t_auth.is_some()
                && !merged.is_empty()
                && !merged.starts_with('/')
            {
                format!("/{merged}")
            } else {
                merged
            };
            t_path = uri_remove_dot_segments(&merged);
        }
    }
    Ok(uri_recompose(
        &b_scheme, &t_auth, &t_path?, &r_query, &r_frag,
    ))
}

/// RFC 3986 §5.3 — recompose component parts into a URI string.
/// [`uri_recompose`] in code units. Scheme and authority stay `&str`: they are
/// ASCII by the URI grammar, and `relativize` only ever COMPARES them. That is
/// the audit's own rule from G78-1 — a value that is inspected may stay text;
/// only what is handed back needs units.
pub fn uri_recompose_units(
    scheme: &Option<String>,
    authority: &Option<String>,
    path: &[u16],
    query: &Option<Vec<u16>>,
    fragment: &Option<Vec<u16>>,
) -> Vec<u16> {
    let mut s: Vec<u16> = Vec::new();
    if let Some(sc) = scheme {
        s.extend(sc.encode_utf16());
        s.push(u16::from(b':'));
    }
    if let Some(a) = authority {
        s.extend("//".encode_utf16());
        s.extend(a.encode_utf16());
    }
    s.extend_from_slice(path);
    if let Some(q) = query {
        s.push(u16::from(b'?'));
        s.extend_from_slice(q);
    }
    if let Some(f) = fragment {
        s.push(u16::from(b'#'));
        s.extend_from_slice(f);
    }
    s
}

pub fn uri_recompose(
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

/// Is every `java.net.URI` this VM hands out the JDK's own model: `--real-jdk`,
/// or `--compatible` over a real JDK with the `jdk-uri` HotSpot-alignment row
/// (nio21 wave 3, L1; default ON since nio21 wave 4, `CRATONVM_COMPAT_ALIGN=-jdk-uri`
/// reverts it)? Then the
/// `URI` stand-ins are not registered (`retired_shadow::COMPAT_FAMILIES`), so
/// real `URI` bytecode is the only reader of a URI's fields, and every
/// producer below builds or adopts the JDK parser's fields instead of
/// publishing its own split. One field load and one bit test; per VM.
#[inline]
pub fn jdk_uri_model(ctx: &dyn NativeContext) -> bool {
    ctx.aligned(cratonvm_types::HotSpotAlignment::JdkUri)
}

/// `--real-jdk` (or the `jdk-uri` row, [`jdk_uri_model`]): `text` as a
/// `java.net.URI` built by the JDK's own `URI(String)` constructor. `None`
/// outside that model, or when that constructor refuses `text`, in which case
/// the caller mints one as before.
///
/// SBR wave 1 (lane `uri`) retired every `java/net/URI` bridge, so under
/// `--real-jdk` real `URI` bytecode is the only reader of a URI's fields --
/// and it reads fields this VM's producers either never wrote or wrote under a
/// different convention: `decodedPath` from a lossy `&str` while `path` was
/// re-derived from UTF-16 units (`RJdkBridge1` `surrog`, `URL.toURI().getPath()`
/// with a lone surrogate, measured under the dial), and an opaque URI's `path`
/// set to its scheme-specific part where the JDK's `isOpaque()` is literally
/// `path == null`. A producer that runs the JDK's own parser cannot disagree
/// with the JDK's own readers. The sibling for `URL` is
/// `jboss_module_loader::jdk_only_constructed_url`.
pub fn jdk_only_constructed_uri(
    ctx: &mut dyn NativeContext,
    text: ObjectRef,
) -> Option<ObjectRef> {
    if !jdk_uri_model(&*ctx) {
        return None;
    }
    match ctx.new_object_initialized(
        "java/net/URI",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(text))],
    ) {
        Ok(Some(Value::Object(Some(uri)))) => Some(uri),
        _ => None,
    }
}

/// [`jdk_only_constructed_uri`] over a Rust string. Callers that hold the
/// original `String` object should pass that instead: a `&str` cannot carry
/// an unpaired surrogate.
pub fn jdk_only_constructed_uri_str(
    ctx: &mut dyn NativeContext,
    raw: &str,
) -> Option<ObjectRef> {
    if !jdk_uri_model(&*ctx) {
        return None;
    }
    let text = ctx.create_string_uninterned(raw);
    jdk_only_constructed_uri(ctx, text)
}

/// `--real-jdk`: give the ALREADY-ALLOCATED `uri_obj` the state the JDK's
/// `URI(String)` constructor gives `raw`, by building one through that
/// constructor and copying its instance slots across. `true` when it did.
///
/// For the producers that allocate first and publish second (`Path.toUri`,
/// `HttpRequestImpl.uri`, `URL.toURI`'s fallback, ...): the object they
/// return keeps its identity, and every field real `URI` bytecode reads is the
/// one the real parser wrote. A refusal leaves `uri_obj` untouched -- the
/// constructor ran on a DIFFERENT object -- so the caller's own publish is
/// still a clean fallback, which running `<init>` on `uri_obj` itself would
/// not be (the parser assigns fields as it goes, and throws midway).
pub fn uri_adopt_jdk_parse(ctx: &mut dyn NativeContext, uri_obj: ObjectRef, raw: &str) -> bool {
    if !jdk_uri_model(&*ctx) || uri_has_synthetic_layout(ctx, uri_obj) {
        return false;
    }
    let pin = ctx.pin_native_root(uri_obj);
    let parsed = jdk_only_constructed_uri_str(ctx, raw);
    let dst = ctx.read_native_pin(pin, uri_obj);
    ctx.unpin_native_roots(pin);
    let Some(src) = parsed else {
        return false;
    };
    if ctx.class_id_of_object(src) != ctx.class_id_of_object(dst) {
        return false;
    }
    let n = ctx.object_num_fields(src).min(ctx.object_num_fields(dst));
    for i in 0..n {
        let v = ctx.get_field(src, i);
        ctx.set_field(dst, i, v);
    }
    true
}

/// Allocate a synthetic `java.net.URI` carrying `raw` as its full text.
///
/// Fields are populated **by name** so the object is consistent with the
/// real `java.net.URI` slot layout — writing by raw slot index would clobber
/// e.g. the `path` field (real URI slot 6) with the full URI string and
/// break `getPath()`/`new File(URI)`.
pub fn make_uri(ctx: &mut dyn NativeContext, raw: &str) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(uri) = jdk_only_constructed_uri_str(ctx, raw) {
        return Ok(uri);
    }
    let uri_obj = try_alloc_concurrent_synthetic(ctx, "java/net/URI", 18)?;
    // gc-common w19-d handoff: `uri_publish_named` allocates a dozen Strings
    // and roots the URI only inside itself; hand back its address after them.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let uri_h = scope.root(uri_obj);
    uri_publish_named(&mut *scope, uri_obj, raw, None);
    Ok(scope.get(&uri_h))
}

/// [`make_uri`] from code units.
///
/// `uri_publish_named` takes a `&str`, so it is handed the lossy spelling to
/// set the ASCII-only components (scheme, authority, port) and the three
/// text-carrying ones are then rewritten from the units. This is the same
/// correction `URL.toURI()` applies, for the same reason, and is deliberately
/// spelled the same way so the two cannot drift.
pub fn make_uri_units(ctx: &mut dyn NativeContext, raw: &[u16]) -> Result<ObjectRef, MethodCallFailed> {
    // `--real-jdk` (or the `jdk-uri` row): parse the UNITS, so no component is
    // ever derived from the lossy spelling below (see `jdk_only_constructed_uri`).
    if jdk_uri_model(&*ctx) {
        let text = ctx.create_string_from_units(raw);
        if let Some(uri) = jdk_only_constructed_uri(ctx, text) {
            return Ok(uri);
        }
    }
    let uri_obj = try_alloc_concurrent_synthetic(ctx, "java/net/URI", 18)?;
    let lossy = String::from_utf16_lossy(raw);
    // gc-common w19-d handoff: rooted across the publish and each String
    // below, and every store goes through the URI's current address.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let uri_h = scope.root(uri_obj);
    uri_publish_named(&mut *scope, uri_obj, &lossy, None);
    let raw_obj = scope.create_string_from_units(raw);
    let uri_now = scope.get(&uri_h);
    scope.set_field_by_name(uri_now, "string", Value::Object(Some(raw_obj)));
    if let Some(pu) = uri_select_raw_path_units(raw) {
        let o = scope.create_string_from_units(&pu);
        let uri_now = scope.get(&uri_h);
        scope.set_field_by_name(uri_now, "path", Value::Object(Some(o)));
    }
    if let Some(q) = uri_query_units(raw) {
        let o = scope.create_string_from_units(&q);
        let uri_now = scope.get(&uri_h);
        scope.set_field_by_name(uri_now, "query", Value::Object(Some(o)));
    }
    if let Some(f) = uri_fragment_units(raw) {
        let o = scope.create_string_from_units(&f);
        let uri_now = scope.get(&uri_h);
        scope.set_field_by_name(uri_now, "fragment", Value::Object(Some(o)));
    }
    Ok(scope.get(&uri_h))
}

/// Write a URI's components into `uri_obj` under the names the real
/// `java.net.URI` declares.
///
/// JDK-ONLY-LAYOUT. Eleven sites in four files allocate a `java/net/URI` and
/// then stamp its components in by RAW SLOT INDEX, under three mutually
/// inconsistent fabricated models (`raw@0 scheme@1 path@4`, `raw@0 raw@4`, and
/// the `scheme@0 host@1 port@2 path@3 query@4` block that `http2.rs` and
/// `servlet.rs` used to read). In real-JDK mode every one of those objects is a
/// genuine `java.net.URI` whose slots are `scheme, fragment, authority,
/// userInfo, host, port, path, query`, so each write landed on a different
/// field than it named. It went unnoticed because the accessors read the SAME
/// wrong slots back: writer and reader agreed, and the object was wrong only
/// from real bytecode's point of view — which is precisely what
/// `URI.getAuthority()` is.
///
/// `path_override` is for the callers that know a better path than the generic
/// split produces (a `jar:` inner entry, a Windows drive-letter path).
///
/// This function writes ONLY by name. Its callers keep their raw-slot writes
/// for a receiver that genuinely has a fabricated layout, asked by NAME through
/// [`uri_has_synthetic_layout`].
pub fn uri_publish_named(
    ctx: &mut dyn NativeContext,
    uri_obj: ObjectRef,
    raw: &str,
    path_override: Option<&str>,
) {
    // `--real-jdk`: the JDK's own parse, not this function's. See
    // `uri_adopt_jdk_parse` -- `path_override` is deliberately not consulted
    // there, because the real parse of `raw` is what HotSpot's producer of the
    // same text (`new URI(s)`) would hold.
    if uri_adopt_jdk_parse(ctx, uri_obj, raw) {
        return;
    }
    let (scheme, authority, path, query, fragment) = uri_split(raw);
    let path = path_override.map_or(path, str::to_string);
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
    // Pin across the whole publish. Every `put` below allocates a String, and a
    // moving young GC there relocates `uri_obj` and leaves this raw ref stale —
    // it then resolves to a reused, usually `java/lang/Object`, slot, and the
    // component lands on a stranger. `make_uri` carried that exposure for one
    // caller; this function now has eleven, so the pin belongs here rather than
    // at each of them. `read_native_pin` is handle-authoritative: the stale
    // local is only its out-of-range fallback.
    let pin = ctx.pin_native_root(uri_obj);
    let put = |ctx: &mut dyn NativeContext, name: &str, v: &str| {
        let s = ctx.create_string(v);
        let obj = ctx.read_native_pin(pin, uri_obj);
        ctx.set_field_by_name(obj, name, Value::Object(Some(s)));
    };
    let put_opt = |ctx: &mut dyn NativeContext, name: &str, val: &Option<String>| {
        if let Some(v) = val {
            let s = ctx.create_string(v);
            let obj = ctx.read_native_pin(pin, uri_obj);
            ctx.set_field_by_name(obj, name, Value::Object(Some(s)));
        }
    };

    // `string` — the volatile full-text cache `uri_raw_string` reads first.
    put(ctx, "string", raw);
    put_opt(ctx, "scheme", &scheme);
    put_opt(ctx, "authority", &authority);
    put_opt(ctx, "query", &query);
    put_opt(ctx, "fragment", &fragment);
    // `path` is the RAW component and `decodedPath` is its percent-decoded
    // twin -- `getRawPath()` reads the first, `getPath()` the second. Writing
    // one string to both made whichever accessor disagreed with it wrong, and
    // silently: every path without an escape renders identically, so this only
    // shows on a path that actually needs encoding.
    //
    // MEASURED (`probes/UriRawPathProbe.java`), a Path holding a space:
    //
    //     HotSpot   Path.toUri().getRawPath() = /tmp/junit-123/te%20st.jar
    //     CratonVM                            = /tmp/junit-123/te st.jar
    //
    // while `toUri().toString()` and `new URI(s).getRawPath()` were both already
    // right -- so the URI was self-inconsistent depending on how it was built.
    // Spring Boot's `NestedPath.toUri()` concatenates `getRawPath()` into a new
    // URI, so the un-escaped space came back as
    // `URISyntaxException: Illegal character in path at index 40`
    // (`boot.loader.nio.file.NestedPathTests.toUriWhenHasSpecialCharsReturnsEncodedUri`).
    //
    // Decoding here rather than asking callers for both forms keeps the eleven
    // call sites unchanged: a caller that already passes a RAW override now gets
    // a correct `decodedPath` for free, and one that passes an
    // already-decoded override is no worse off than before (decoding a decoded
    // path is a no-op unless it contains a literal `%`, which such a caller
    // could not have represented correctly either way).
    if !path.is_empty() {
        put(ctx, "path", &path);
        let decoded_path = uri_percent_decode(&path);
        put(ctx, "decodedPath", &decoded_path);
    }
    put(ctx, "schemeSpecificPart", &ssp);
    let decoded_ssp = uri_percent_decode(&ssp);
    put(ctx, "decodedSchemeSpecificPart", &decoded_ssp);
    // `host`, `userInfo` and `port` come out of the authority, and a real
    // `java.net.URI` declares all three. Writing them keeps a receiver that
    // real bytecode reads directly consistent with what the accessors answer.
    let port = match authority.as_deref().filter(|a| !a.is_empty()) {
        Some(a) => {
            let (user_info, host, port) = uri_parse_authority(a);
            if let Some(h) = host {
                put(ctx, "host", &h);
            }
            if let Some(u) = user_info {
                put(ctx, "userInfo", &u);
            }
            port
        }
        None => -1,
    };
    let obj = ctx.read_native_pin(pin, uri_obj);
    ctx.set_field_by_name(obj, "port", Value::Int(port));
    ctx.unpin_native_roots(pin);
}

pub fn register_uri_natives(r: &mut NativeMethodRegistry) {
    let uri = "java/net/URI";

    // toString() -> the cached text, as the OBJECT, not a decoded copy.
    //
    // HotSpot's `URI.toString()` is `string != null ? string : defineString()`
    // -- it hands back the very string it cached. Doing the same here is both
    // exact and free: see `uri_raw_string_object` for the measured reason (a
    // Rust `String` cannot carry an unpaired surrogate, so the copy
    // substituted U+FFFD). The decode fallback stays for a URI with no cached
    // text to hand back.
    r.register(uri, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(s) = uri_raw_string_object(ctx, this) {
            return Ok(Some(Value::Object(Some(s))));
        }
        let s = uri_raw_string(ctx, this);
        Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
    }, NativeKind::SyntheticStub);

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
        // Fast path: OUR model's scheme slot (0) if it was set during
        // construction. Slot 0 happens to be `scheme` on a real `URI` too, but
        // the guard stays: this is the fabricated model's index, and it is the
        // model that must own it.
        if uri_has_synthetic_layout(ctx, this) {
            if let Value::Object(Some(s)) = ctx.get_field(this, 0) {
                if let Some(v) = ctx.read_string(s) {
                    if !v.is_empty() && !v.contains(':') {
                        return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
                    }
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
    }, NativeKind::SyntheticStub);

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
            let raw_u = uri_raw_units(ctx, this);
            let ssp = uri_raw_scheme_specific_part_units(&raw_u);
            let decoded = uri_percent_decode_units(&ssp);
            Ok(Some(Value::Object(Some(
                ctx.create_string_from_units(&decoded),
            ))))
        }, NativeKind::SyntheticStub,
    );

    // getRawSchemeSpecificPart() → same (we don't encode/decode)
    r.register(
        uri,
        "getRawSchemeSpecificPart",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let raw_u = uri_raw_units(ctx, this);
            let ssp = uri_raw_scheme_specific_part_units(&raw_u);
            Ok(Some(Value::Object(Some(
                ctx.create_string_from_units(&ssp),
            ))))
        }, NativeKind::SyntheticStub,
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
        // UNITS from here down (G75-1 N1): this getter RE-PARSES the raw text,
        // so a `read_string` here re-lost the unit no matter what the parse or
        // the component fields held.
        let raw_u = uri_raw_units(ctx, this);
        let raw = String::from_utf16_lossy(&raw_u);
        // Opaque URIs (e.g. `mailto:x@y.com`) have a null path. Decide from the
        // raw text rather than the `path` field, because `make_uri` stores a
        // `path` field for opaque URIs too (from `uri_split`), which would
        // otherwise surface the scheme-specific-part as the path.
        let parsed = match uri_select_raw_path_units(&raw_u) {
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
            // The FIELD is read as units too. It wins over the parsed value
            // below, so reading it as text would have put the loss back after
            // the parse had avoided it.
            if let Some(v) = ctx.read_string_units(s) {
                let relative_without_authority =
                    !raw.starts_with('/') && !raw.starts_with("//") && raw.find(':').is_none();
                if !v.is_empty() && v != raw_u && !(relative_without_authority && v != raw_path) {
                    raw_path = v;
                }
            }
        }
        let decoded = uri_percent_decode_units(&raw_path);
        Ok(Some(Value::Object(Some(
            ctx.create_string_from_units(&decoded),
        ))))
    }, NativeKind::SyntheticStub);

    // getRawPath() → same path selection as getPath() but WITHOUT decoding.
    // Opaque URIs → null; hierarchical authority-only → "".
    r.register(uri, "getRawPath", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw_u = uri_raw_units(ctx, this);
        let raw = String::from_utf16_lossy(&raw_u);
        let parsed = match uri_select_raw_path_units(&raw_u) {
            None => return Ok(Some(Value::Object(None))),
            Some(p) => p,
        };
        // Same slot-collision guard as getPath() above.
        let mut raw_path = parsed;
        if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "path") {
            // The FIELD is read as units too. It wins over the parsed value
            // below, so reading it as text would have put the loss back after
            // the parse had avoided it.
            if let Some(v) = ctx.read_string_units(s) {
                let relative_without_authority =
                    !raw.starts_with('/') && !raw.starts_with("//") && raw.find(':').is_none();
                if !v.is_empty() && v != raw_u && !(relative_without_authority && v != raw_path) {
                    raw_path = v;
                }
            }
        }
        Ok(Some(Value::Object(Some(
            ctx.create_string_from_units(&raw_path),
        ))))
    }, NativeKind::SyntheticStub);

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
        // No authority section in the raw string. java.net.URI parses the host
        // ONLY out of an authority, so there is no host here -- MEASURED
        // 2026-08-13 (/tmp/U.java): `mailto:a@b.com`.getHost() and
        // `a/b`.getHost() are both null on HotSpot, and this fallback answered
        // "mailto:a@b.com" and "a" respectively by reading a slot that means
        // something else on a real receiver. Keep the fallback ONLY when we
        // could not obtain a raw string at all, which is the synthetic-model
        // case it was written for.
        if !raw.is_empty() {
            return Ok(Some(Value::Object(None)));
        }
        // Fall back to an explicit host slot set during construction.
        if uri_has_synthetic_layout(ctx, this) {
            if let Value::Object(Some(s)) = ctx.get_field(this, 1) {
                if let Some(v) = ctx.read_string(s) {
                    if !v.is_empty() {
                        return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
                    }
                }
            }
        } else if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "host") {
            if let Some(v) = ctx.read_string(s) {
                if !v.is_empty() {
                    return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
                }
            }
        }
        Ok(Some(Value::Object(None)))
    }, NativeKind::SyntheticStub);

    // getPort() → port field (2) when a real port was stored, else parse the raw
    // authority. Absent port is -1 (java.net.URI contract), not the int-default 0.
    r.register(uri, "getPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // The raw string is the authoritative source, exactly as it is for
        // `getHost` above, and for the same reason: the stored `port` slot is
        // written by several producers that do not all use `java.net.URI`'s
        // grammar. `url_parse` serves `java.net.URL` too, whose port production
        // is `Integer.parseInt` and therefore accepts a leading `+` — MEASURED,
        // `new URL("http://h:+80/p").getPort()` is 80 on HotSpot while
        // `new URI("http://h:+80/p").getPort()` is -1. While this read came
        // first, guarded only by `if p > 0`, a URL-shaped 80 in the slot
        // masked the URI answer; and for an authority-less `urn:isbn:0451450523`
        // the same slot held 451450523. Deriving from the raw string keeps
        // `getPort` and `getHost` on ONE model, so they cannot disagree about
        // whether the authority demoted.
        let raw = uri_raw_string(ctx, this);
        if !raw.is_empty() {
            if let (_, Some(auth), _, _, _) = uri_split(&raw) {
                let (_, _, port) = uri_parse_authority(&auth);
                return Ok(Some(Value::Int(port)));
            }
            // A raw string with no `//authority` has no port at all.
            return Ok(Some(Value::Int(-1)));
        }
        // No raw string: a purely synthetic receiver. Slot 2 is OUR model's
        // `port`; on a real `java.net.URI` it is `authority`, a `String`, so
        // this read used to answer a reference where an `Int` was expected on
        // every real-layout receiver.
        if uri_has_synthetic_layout(ctx, this) {
            if let Value::Int(p) = ctx.get_field(this, 2) {
                if p > 0 {
                    return Ok(Some(Value::Int(p)));
                }
            }
        } else if let Value::Int(p) = ctx.get_field_by_name(this, "port") {
            if p > 0 {
                return Ok(Some(Value::Int(p)));
            }
        }
        Ok(Some(Value::Int(-1)))
    }, NativeKind::SyntheticStub);

    // getAuthority() / getRawAuthority() → the `//authority` component.
    //
    // These were the two accessors in the family with NO native, so they fell
    // through to real `java.net.URI` bytecode, which reads the `authority`
    // field — and `native_uri_init`/`uri_store_named` never wrote it. Measured
    // 2026-08-10 against Temurin 25.0.3:
    // `new URI("http://user:pw@example.com:8080/a/b?q=1#frag").getAuthority()`
    // answered **null** where HotSpot answers `user:pw@example.com:8080`. It is
    // the same shape as `getUserInfo` before it was registered, and the same
    // parse: authority is whatever `uri_split` puts between `//` and the path.
    //
    // `getAuthority` returns the DECODED form and `getRawAuthority` the literal
    // one, exactly as the JDK's `decodedAuthority`/`authority` pair does; for an
    // authority with no escapes the two coincide.
    r.register(uri, "getAuthority", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(match uri_authority_component(ctx, this) {
            Some(a) => {
                let decoded = uri_percent_decode(&a);
                Value::Object(Some(ctx.create_string(&decoded)))
            }
            None => Value::Object(None),
        }))
    }, NativeKind::SyntheticStub);
    r.register(
        uri,
        "getRawAuthority",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(match uri_authority_component(ctx, this) {
                Some(a) => Value::Object(Some(ctx.create_string(&a))),
                None => Value::Object(None),
            }))
        }, NativeKind::SyntheticStub,
    );

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
                return Ok(Some(Value::Object(Some(ctx.create_string(&decoded)))));            }        }
        Ok(Some(Value::Object(None)))
    }, NativeKind::SyntheticStub);

    // getQuery() → decoded hierarchical query. Opaque URIs keep a literal '?'
    // inside the scheme-specific part, so parse from `uri_split` rather than a
    // raw `find('?')` fallback.
    r.register(uri, "getQuery", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // `uri_query_units` rather than `uri_split`: the query is "between the
        // first `?` and the first `#` after it", which `uri_split` also
        // computes, but only the units spelling can carry the result back. The
        // 26-row probe asserts the two agree on every ordinary URI.
        let raw_u = uri_raw_units(ctx, this);
        if !raw_u.is_empty() {
            return match uri_query_units(&raw_u) {
                Some(q) => {
                    let decoded = uri_percent_decode_units(&q);
                    Ok(Some(Value::Object(Some(
                        ctx.create_string_from_units(&decoded),
                    ))))
                }
                None => Ok(Some(Value::Object(None))),
            };
        }
        // Last-ditch field fallback for any pre-populated real-JDK URI object
        // that has a query field but no raw-string cache visible to CratonVM.
        if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "query") {
            if let Some(v) = ctx.read_string(s) {
                let decoded = uri_percent_decode(&v);
                return Ok(Some(Value::Object(Some(ctx.create_string(&decoded)))));            }        }
        Ok(Some(Value::Object(None)))
    }, NativeKind::SyntheticStub);

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
        let raw_u = uri_raw_units(ctx, this);
        match uri_fragment_units(&raw_u) {
            Some(f) => {
                let decoded = uri_percent_decode_units(&f);
                Ok(Some(Value::Object(Some(
                    ctx.create_string_from_units(&decoded),
                ))))            }
            None => Ok(Some(Value::Object(None))),
        }
    }, NativeKind::SyntheticStub);

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
        }, NativeKind::SyntheticStub,
    );

    // getRawQuery() → raw hierarchical query, else null. For opaque URIs,
    // '?' is part of the raw scheme-specific part and must not surface here.
    r.register(uri, "getRawQuery", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw_u = uri_raw_units(ctx, this);
        if !raw_u.is_empty() {
            return match uri_query_units(&raw_u) {
                Some(q) => Ok(Some(Value::Object(Some(ctx.create_string_from_units(&q))))),
                None => Ok(Some(Value::Object(None))),
            };
        }
        match ctx.get_field_by_name(this, "query") {
            Value::Object(Some(s)) => Ok(Some(Value::Object(Some(s)))),
            _ => Ok(Some(Value::Object(None))),
        }
    }, NativeKind::SyntheticStub);

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
        }, NativeKind::SyntheticStub,
    );

    // isAbsolute() → true if scheme is non-null.
    //
    // "Has a colon somewhere" is NOT that test: a colon inside a relative
    // reference's first path segment is an ordinary character, which is why
    // `uri_scheme_colon` also demands an ALPHA start and an alphanumeric/`+-.`
    // body before the colon. MEASURED on HotSpot 25:
    // `URI.create("/redirect:account?q=1")` is neither absolute nor opaque,
    // and both answered `true` here (`probes/OpaqueUriProbe.java` row H11) —
    // the same relative-path-with-colon shape `uri_scheme_colon` was written
    // for when it was added for Spring's redirect view names.
    r.register(uri, "isAbsolute", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        let is_abs = uri_scheme_colon(&raw).is_some();
        Ok(Some(Value::Int(if is_abs { 1 } else { 0 })))
    }, NativeKind::SyntheticStub);

    // isOpaque() → absolute, with a scheme-specific part that does not start
    // with '/'. Delegated to `uri_text_is_opaque` so this can never disagree
    // with the null-path rule `getPath`/`getRawQuery` are built on.
    r.register(uri, "isOpaque", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = uri_raw_string(ctx, this);
        Ok(Some(Value::Int(if uri_text_is_opaque(&raw) {
            1
        } else {
            0
        })))
    }, NativeKind::SyntheticStub);

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
        // `--real-jdk`: the JDK's own constructor for these too, so the URL
        // carries its handler (`file:`/`jar:`/`jrt:`), as `URI.toURL()` does
        // on HotSpot. Its `file` is the same scheme-specific part the build
        // below writes.
        if let Some(url) = crate::jboss_module_loader::jdk_only_constructed_url(ctx, &raw) {
            return Ok(Some(Value::Object(Some(url))));
        }
        // Build a simple 13-field synthetic URL (same layout as p59_alloc_url).
        //
        // gc-common w20-d: the Strings first, each rooted, and the URL last.
        // The URL was held raw across all six String allocations, and each
        // String across the ones after it.
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
        let mut scope = NativeHandleScope::new(ctx);
        let proto_s = scope.create_string(proto);
        let proto_h = scope.root(proto_s);
        let file_s = scope.create_string(file_without_ref);
        let file_h = scope.root(file_s);
        let path_s = scope.create_string(path);
        let path_h = scope.root(path_s);
        let query_h = match query_part.filter(|query| !query.is_empty()) {
            Some(query) => {
                let s = scope.create_string(query);
                Some(scope.root(s))
            }
            None => None,
        };
        let ref_h = match ref_part.filter(|fragment| !fragment.is_empty()) {
            Some(fragment) => {
                let s = scope.create_string(fragment);
                Some(scope.root(s))
            }
            None => None,
        };
        let host_s = scope.create_string("");
        let host_h = scope.root(host_s);
        let url = try_alloc_concurrent_synthetic(&mut *scope, "java/net/URL", 13)?;
        let proto_s = scope.get(&proto_h);
        let file_s = scope.get(&file_h);
        let path_s = scope.get(&path_h);
        let query_s = query_h.as_ref().map(|h| scope.get(h));
        let ref_s = ref_h.as_ref().map(|h| scope.get(h));
        let host_s = scope.get(&host_h);
        let ctx = &mut *scope;
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
        crate::jboss_module_loader::url_write_declared_field_defaults(&*ctx, url);
        Ok(Some(Value::Object(Some(url))))
    }, NativeKind::SyntheticStub);

    // compareTo(URI) -> component-wise ordering consistent with equals.
    r.register(uri, "compareTo", "(Ljava/net/URI;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Err(uri_compare_to_null()),
        };
        Ok(Some(Value::Int(uri_compare(ctx, this, other))))
    }, NativeKind::SyntheticStub);

    // Bridge form used by erased Comparable call sites.
    r.register(uri, "compareTo", "(Ljava/lang/Object;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Err(uri_compare_to_null()),
        };
        Ok(Some(Value::Int(uri_compare(ctx, this, other))))
    }, NativeKind::SyntheticStub);

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
    }, NativeKind::SyntheticStub);

    // hashCode() — must agree with `equals` (see `uri_hash_code`): hashing the
    // raw string breaks the equals/hashCode contract for URIs that differ
    // only by scheme/host case, since those compare equal.
    r.register(uri, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(uri_hash_code(ctx, this))))
    }, NativeKind::SyntheticStub);

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
        let mut norm = uri_remove_dot_segments(&path)?;
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
        Ok(Some(Value::Object(Some(make_uri(ctx, &recomposed)?))))
    }, NativeKind::SyntheticStub);

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
                // A null argument is a NullPointerException, not the
                // receiver. The JDK dereferences the argument's own fields
                // before any resolution happens, so answering `this` is a
                // fabricated success a caller cannot tell from a no-op.
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: None,
                    }
                    .into())
                }
            };
            let resolved = uri_resolve_ref(&base, &reference)?;
            Ok(Some(Value::Object(Some(make_uri(ctx, &resolved)?))))
        }, NativeKind::SyntheticStub,
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
            let resolved = uri_resolve_ref(&base, &reference)?;
            Ok(Some(Value::Object(Some(make_uri(ctx, &resolved)?))))
        }, NativeKind::SyntheticStub,
    );

    // parseServerAuthority() -> `this` when the authority is server-based, and
    // a URISyntaxException naming the offending character when it is not.
    //
    // It had NO registration, so real `java.net.URI` bytecode ran against a
    // synthetic receiver whose parsed fields it could not see, and the method
    // silently answered the receiver for every input. MEASURED against HotSpot
    // 25.0.4+7 by `apps/probes/UriRecompositionSweep.java`:
    // `URI.create("http://host:x/a").parseServerAuthority()` is
    // `Illegal character in port number at index 12` there and was the URI
    // itself here — a refusal turned into a value, which is the shape a caller
    // uses this method precisely to avoid.
    r.register(
        uri,
        "parseServerAuthority",
        "()Ljava/net/URI;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let raw = uri_raw_string(ctx, this);
            if let Some(fail) = crate::uri_server_authority_fail(&raw) {
                if let Some(exc) = crate::uri_syntax_exception_pub(ctx, &raw, &fail) {
                    return Err(exc);
                }
            }
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
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
                // A null argument is a NullPointerException, not the
                // receiver. The JDK dereferences the argument's own fields
                // before any resolution happens, so answering `this` is a
                // fabricated success a caller cannot tell from a no-op.
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: None,
                    }
                    .into())
                }
            };
            // G75-1 N3, the last diverging row of that probe. `relativize` is
            // not an accessor: it REBUILDS a path from segments rather than
            // selecting a range of one, which is why it outlived the accessor
            // conversion.
            //
            // Scheme and authority stay text — ASCII by grammar, and only
            // COMPARED below. BOTH paths are carried in units, not just the
            // target's: the base path is prefix-matched against the target, so
            // a lossy base would fail to match a target that legitimately
            // starts with it.
            let base_u = uri_raw_units(ctx, this);
            let base = String::from_utf16_lossy(&base_u);
            let target_u = uri_raw_units(ctx, other);
            let target = String::from_utf16_lossy(&target_u);
            let (b_scheme, b_auth, b_path, _b_query, _b_frag) = uri_split(&base);
            let (t_scheme, t_auth, t_path, _t_q_text, _t_f_text) = uri_split(&target);
            let b_path_u = uri_select_raw_path_units(&base_u).unwrap_or_default();
            let t_path_u = uri_select_raw_path_units(&target_u).unwrap_or_default();
            let t_query = uri_query_units(&target_u);
            let t_frag = uri_fragment_units(&target_u);
            let b_opaque = b_scheme.is_some() && b_auth.is_none() && !b_path.starts_with('/');
            let t_opaque = t_scheme.is_some() && t_auth.is_none() && !t_path.starts_with('/');
            if b_opaque
                || t_opaque
                || !opt_str_eq_ignore_case(&b_scheme, &t_scheme)
                || b_auth != t_auth
            {
                return Ok(Some(Value::Object(Some(other))));
            }
            let b_norm = uri_remove_dot_segments_units(&b_path_u)?;
            let t_norm = uri_remove_dot_segments_units(&t_path_u)?;
            if !t_norm.starts_with(&b_norm) {
                return Ok(Some(Value::Object(Some(other))));
            }
            let slash = u16::from(b'/');
            let rel = &t_norm[b_norm.len()..];
            if rel.is_empty() {
                return Ok(Some(Value::Object(Some(make_uri(ctx, "")?))));
            }
            if b_norm.last() != Some(&slash) && rel.first() != Some(&slash) {
                return Ok(Some(Value::Object(Some(other))));
            }
            let rel = if rel.first() == Some(&slash) {
                &rel[1..]
            } else {
                rel
            };
            let recomposed = uri_recompose_units(&None, &None, rel, &t_query, &t_frag);
            Ok(Some(Value::Object(Some(make_uri_units(ctx, &recomposed)?))))
        }, NativeKind::SyntheticStub,
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
                // G14-1 N4. `URI.create(null)` is `new URI(null)` with the
                // checked exception translated, and `new URI(null)` opens by
                // dereferencing its own `input` field. MEASURED on HotSpot
                // 25.0.3+9-LTS:
                //
                //   URI.create(null)
                //     NullPointerException: Cannot invoke "String.length()"
                //                           because "this.input" is null
                //
                // This answered `null` instead, so the caller learned nothing
                // here and NPE'd later at its own call site naming a
                // different method — the shape a refusal must never take. The
                // text is the ONE spelling `net_uri_inet::native_uri_init`
                // already throws for the constructor, deliberately shared so
                // the two doors cannot drift apart: `create` is specified as
                // that constructor, not as a second parser.
                //
                // A MISSING argument (`args` empty) stays a dispatch defect
                // and keeps the old return.
                Some(Value::Object(None)) => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some(
                            "Cannot invoke \"String.length()\" because \"this.input\" is null"
                                .into(),
                        ),
                    }
                    .into())
                }
                _ => return Ok(Some(Value::Object(None))),
            };
            let s = ctx.read_string(s_obj).unwrap_or_default();
            // `URI.create(String)` is `new URI(str)` with the checked
            // exception TRANSLATED, not replaced. Two things came out of that
            // word, and this door had neither: the same refusals in the same
            // order (it carried its own four-of-seven transcription with a
            // catch-all `Illegal character in URI at index 9` where the
            // constructor names the component), and the `URISyntaxException`
            // itself as the `IllegalArgumentException`'s CAUSE — which is the
            // only place the reason and index survive for a caller that
            // catches the unchecked wrapper.
            if let Some(fail) = crate::uri_parse_fail(&s) {
                return Err(uri_create_iae(ctx, &s, &fail));
            }
            Ok(Some(Value::Object(Some(make_uri(ctx, &s)?))))
        }, NativeKind::SyntheticStub,
    );
}

// ===========================================================================
// RE.1 — java.net.Socket
// ===========================================================================

/// How long a parked `SocketInputStream.read` waits inside one poll before
/// re-asking the registry whether its socket was closed under it.
///
/// A liveness bound, not a latency cost: the poll returns the instant the
/// socket becomes readable, so payload is never delayed by it — it only bounds
/// how long a reader stays parked after another thread calls `Socket.close()`.
/// Same value and same role as `native-io/src/net.rs`'s
/// `NET_READ_CLOSE_POLL_MS` and `socket_channel.rs`'s `READ_CLOSE_POLL_MS`,
/// because this is the same loop.
pub const RE1_READ_CLOSE_POLL_MS: i32 = 25;

/// The error a parked read reports once its socket has been closed from
/// another thread. [`re1_socket_read_stream`] maps it to a real
/// `java.net.SocketException`.
///
/// `ErrorKind::Interrupted` is unambiguous at this site because
/// [`re1_read_retry_eintr`] reissues every real EINTR and
/// [`re1_socket_poll_readable`] reports one as "not ready" rather than as an
/// error, so nothing else in this path can produce it. That is the same
/// argument `socket_channel.rs::channel_async_closed_err` makes for the
/// channel side.
pub fn re1_socket_closed_err() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "socket closed")
}

/// Is `sid` still a live stream?
///
/// [`re1_close_socket`] *removes* the entry from `s2_registry().streams` (and
/// `take_raw_socket_stream_for_tls` moves it out), so this flips exactly when
/// Java closed the socket. The `net.rs` twin, `net_stream_still_registered`,
/// tests for a `NetSocketHandle::Closed` marker instead only because that
/// registry keeps closed slots; the question asked is identical.
///
/// The lock is taken for the map lookup alone and is never held across the
/// poll or the read — the whole reason [`re1_socket_read_stream`] clones the
/// `Arc` out in the first place (BUG-04 loopback hang).
pub fn re1_stream_still_registered(sid: i32) -> bool {
    s2_registry().lock().streams.contains_key(&sid)
}

/// One `recv`, reissued for as long as it reports EINTR.
///
/// EINTR must never escape to Java: this VM signals its own threads
/// (`jit::xt_root_scan` SIGUSR2s every thread for a cross-thread root scan),
/// so a bare `read` surfacing `Interrupted` would look to callers like a
/// random mid-request connection abort.
pub fn re1_read_retry_eintr(stream: &TcpStream, buf: &mut [u8]) -> std::io::Result<usize> {
    // The std impl is `impl Read for &TcpStream`, so this reads through a
    // shared stream without excluding a peer writer on the same fd.
    let mut r = stream;
    loop {
        match r.read(buf) {
            Err(e) if is_eintr(&e) => continue,
            other => return other,
        }
    }
}

/// A blocking `SocketInputStream.read` that observes an asynchronous
/// `Socket.close()`.
///
/// # Why the plain blocking read could not
///
/// `Socket.close()` on another thread reaches [`re1_close_socket`], which drops
/// the registry's `Arc` and issues `shutdown(Both)` — but it does **not** close
/// the OS handle, because this reader is holding an `Arc` clone of the very
/// `TcpStream` the map dropped. HotSpot's answer to the same situation is
/// `closesocket()` underneath the blocked `recv` (Windows) or `dup2` of a
/// pre-closed descriptor plus a signal (Unix, `NativeDispatcher.preClose`).
/// Neither is expressible over an `Arc<TcpStream>` without closing a handle
/// another thread is mid-syscall on, which is a use-after-close the moment the
/// OS recycles the number.
///
/// `shutdown` is not a substitute, and the platform split is what made this
/// look like it was already fixed: on Linux `SHUT_RD` does wake a parked `recv`
/// — with EOF, so the reader answered `-1` rather than throwing — while Winsock
/// has no `shutdown` that aborts a pending blocking call at all, so on Windows
/// the reader simply never returned.
///
/// So park in `poll` instead of in `recv` and re-ask the registry every
/// [`RE1_READ_CLOSE_POLL_MS`]. That is not a new mechanism: it is the loop
/// `net.rs::net_read_close_aware` and `socket_channel.rs::read_close_aware`
/// landed on 2026-08-07 for the two *real* socket surfaces, and the loop
/// [`re2_accept_into`] in this same file has used on the accept side since the
/// MockWebServer teardown fix. This is the third reader that needed it and the
/// one the 2026-08-07 pass missed, because it is the *synthetic*
/// `java.net.Socket` surface (`CRATONVM_SYNTHETIC_NET_SOCKETS`) rather than
/// either of the two the failing tests reached.
///
/// # `deadline` is not optional decoration
///
/// `Some(dl)` is a live `SO_TIMEOUT`. A wakeup that silently swallowed the
/// caller's timeout would be the same defect pointed the other way: a
/// `setSoTimeout(n)` reader would park past its own deadline forever. So the
/// poll slice is clamped to the time remaining and an expired deadline returns
/// `TimedOut` — which [`re1_socket_read_stream`] already maps to
/// `SocketTimeoutException` — rather than merely declining to poll again. The
/// socket's own `SO_RCVTIMEO` (applied at connect) stays set and remains the
/// first line; this deadline is what still ends the park on a platform or a
/// socket state where `SO_RCVTIMEO` does not fire.
/// `pub` for `phases_early.rs`'s `java/net/SocketInputStream`, which is
/// the FOURTH surface of this defect (W2-2). It keys the same `s2_registry()`
/// `streams` map with the same `sid`, so the mechanism transfers with no
/// adaptation — which is the reason that surface was converged onto this
/// function rather than growing a fifth copy of the loop or being deleted.
pub fn re1_read_close_aware(
    sid: i32,
    stream: &TcpStream,
    buf: &mut [u8],
    deadline: Option<std::time::Instant>,
) -> std::io::Result<usize> {
    loop {
        let slice = match deadline {
            None => RE1_READ_CLOSE_POLL_MS,
            Some(dl) => {
                let left = dl.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Socket read timed out",
                    ));
                }
                // Floor of 1 ms so a sub-millisecond remainder polls once more
                // instead of spinning on a 0 ms timeout.
                left.as_millis().clamp(1, RE1_READ_CLOSE_POLL_MS as u128) as i32
            }
        };
        let ready = match re1_socket_poll_readable(stream, slice) {
            Some(result) => result?,
            // No poll primitive on this target: fall back to the pre-fix
            // blocking read, which cannot see the close but at least still
            // transfers bytes. Spinning on a stub that can never report
            // readiness would be strictly worse than the bug.
            None => return re1_read_retry_eintr(stream, buf),
        };
        // Asked AFTER the poll, so a close landing while we are parked is seen
        // on the very next pass — and a close that raced a readiness edge still
        // wins. That race is safe by construction rather than by luck:
        // `re1_close_socket` removes the registry entry FIRST and only then
        // issues `shutdown(Both)`, so by the time the shutdown's own readiness
        // edge wakes this poll the entry is already gone. Without this check
        // the Linux path would answer such a read with `-1` — a clean
        // end-of-stream — where `Socket.close()` mandates a `SocketException`.
        if !re1_stream_still_registered(sid) {
            return Err(re1_socket_closed_err());
        }
        if !ready {
            continue;
        }
        return re1_read_retry_eintr(stream, buf);
    }
}

/// Build a real `java.net.SocketException` carrying `message`.
///
/// The concrete type is load-bearing, not decoration. JDK 25's
/// `java.net.Socket.close()` specifies that "Any thread currently blocked in an
/// I/O operation upon this socket will throw a SocketException", and callers
/// catch that type; a `java.io.IOException` whose message merely mentions the
/// name walks straight past `catch (SocketException e)`. `RuntimeError` has no
/// `SocketException` variant — only its `ConnectException`/`BindException`/
/// `SocketTimeoutException` subclasses — so the object has to be constructed,
/// exactly as [`re1_socket_write_stream`] below already does for a peer reset.
/// Falls back to a plain IOException if the class cannot be built.
/// `pub` for the same reason as [`re1_read_close_aware`]: the close
/// carrier is worthless without the concrete `java.net.SocketException` at the
/// end of it, and `RuntimeError` has no variant for that type.
pub fn re1_socket_exception(ctx: &mut dyn NativeContext, message: &str) -> MethodCallFailed {
    let jmsg = ctx.create_string(message);
    cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "java/net/SocketException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(jmsg))],
        || ioex(message.to_string()),
    )
}

/// `owner` is the reading `Socket`'s key (gc-common w15-a: the streams hold
/// the key, not the object; see `stream_owner_table`).
pub fn re1_socket_read_stream(
    ctx: &mut dyn NativeContext,
    owner: NativeObjKey,
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
    // gc-common w28-a: the three integers are read under the table lock; the
    // whole-row clone (`SockSide` owns a `String` and a `Vec`) cost two heap
    // allocations per call on this path, which `read()I` takes per byte.
    let (stream_id, closed, read_timeout_ms) =
        sock_peek_key(owner, |s| (s.stream_id, s.closed, s.read_timeout_ms));
    if stream_id < 0 {
        // Closed by another thread since the stream found its owner row.
        if closed != 0 {
            return Err(re1_socket_exception(ctx, "Socket closed"));
        }
        return Err(ioex("Socket not connected"));
    }
    // A live `SO_TIMEOUT` has to bound the park, not just the recv — see
    // `re1_read_close_aware`'s note on why the deadline is not optional.
    // Computed here because `sock_get` needs `ctx`, which is not available
    // inside the blocking region below.
    let deadline = (read_timeout_ms > 0)
        .then(|| std::time::Instant::now() + Duration::from_millis(read_timeout_ms as u64));
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
    let dbg = crate::nbflags().dbg_sock;
    if dbg {
        // "parking in poll", not "blocking on recv": since the asynchronous-close
        // fix this read waits on readiness and re-asks the registry every
        // RE1_READ_CLOSE_POLL_MS. A `[dbg-sock] read:` line with no matching
        // `got=`/exception is therefore a reader that outlived its close, which
        // is the exact symptom this line is used to hunt.
        eprintln!("[dbg-sock] read: sid={stream_id} want={ln} (parking in poll...)");
    }
    let mut tmp = vec![0u8; ln];
    let mut blocked_refs = [Value::Object(Some(buf))];
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    // Parks in `poll` rather than in `recv`, so a `Socket.close()` on another
    // thread ends the park. The EINTR retry that used to be written out here
    // now lives in `re1_read_retry_eintr`, which this calls once the poll says
    // the socket is readable.
    let read_result = re1_read_close_aware(stream_id, &stream, &mut tmp, deadline);
    ctx.end_blocking_region_refs(&mut blocked_refs);

    let buf = match blocked_refs[0] {
        Value::Object(Some(o)) => o,
        _ => buf,
    };
    let n = match read_result {
        Ok(n) => n,
        // The socket was closed from another thread while this read was parked.
        // JDK 25 `java.net.Socket.close()`: "Any thread currently blocked in an
        // I/O operation upon this socket will throw a SocketException." The
        // message matches what HotSpot actually produces, which is
        // `NioSocketImpl.endRead`'s `throw new SocketException("Socket closed")`
        // once the close has moved the impl to `ST_CLOSING`.
        //
        // That retyping is why the `net.rs` twin of this fix could get away with
        // returning a generic error: on the real-JDK surface `endRead` runs in
        // the `finally` of `implRead` and overwrites whatever text arrived. Here
        // it cannot — these synthetic natives ARE the impl, `NioSocketImpl` is
        // never on the path, and nothing downstream will name the type for us.
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
            return Err(re1_socket_exception(ctx, "Socket closed"));
        }
        // SO_RCVTIMEO is reported as TimedOut on Windows and often as
        // WouldBlock on Unix, and `re1_read_close_aware` raises TimedOut itself
        // when the `SO_TIMEOUT` deadline expires with the socket still quiet.
        // All three are Java SocketTimeoutException, not EOF and not a generic
        // IOException; callers deliberately catch this concrete type to retry
        // their protocol operation.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            return Err(RuntimeError::SocketTimeoutException {
                message: format!("Socket read timed out: {e}"),
            }
            .into());
        }
        Err(e) => return Err(ioex(format!("Socket read failed: {e}"))),
    };
    if dbg {
        eprintln!("[dbg-sock] read: sid={stream_id} got={n}");
        if crate::nbflags().dbg_sock_bytes && n != 0 {
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

/// Largest payload handed to one `send` while a blocking write is sliced.
///
/// A blocking-mode `send` of N bytes does not return until all N are queued, so
/// an unsliced `write_all` parks inside the syscall for an unbounded time and
/// observes neither the close nor the poll cadence. 8 KiB matches
/// `socket_channel::WRITE_SLICE_MAX`, whose doc comment carries the measured
/// evidence (`SendWake.java`, Windows 11, JDK 25.0.3: a writer parked in a
/// blocking `send` was still parked 6 s after `shutdown(SHUT_WR)`; only closing
/// the handle woke it). The common protocol frame is smaller than one slice and
/// is therefore still issued whole.
pub const RE1_WRITE_SLICE_MAX: usize = 8 * 1024;

/// A blocking `SocketOutputStream.write` that observes an asynchronous
/// `Socket.close()` — the write twin of [`re1_read_close_aware`].
///
/// The reader on this surface was fixed on 2026-08-11 and the writer beside it
/// was not, which left the symmetric hole: a thread parked in `write_all`
/// behind a peer that has stopped reading stays parked through
/// `Socket.close()`, for exactly the reason the reader did —
/// [`re1_close_socket`] removes the registry entry and issues `shutdown(Both)`,
/// but cannot close the OS handle while this writer holds an `Arc` clone of the
/// `TcpStream`.
///
/// # Mechanism, not a second mechanism
///
/// Identical loop to [`re1_read_close_aware`]: poll with a bounded slice,
/// re-ask [`re1_stream_still_registered`] after the poll, return
/// [`re1_socket_closed_err`] once the slot is gone. Only the direction of the
/// poll and the slicing differ.
///
/// # What a partial transfer answers
///
/// A close that lands after some bytes are out returns `Ok(written)`. The
/// caller's contract here is `write(byte[], int, int)`, which the JDK specifies
/// as "writes len bytes" — so a short answer is reported to Java as the
/// `SocketException` the close mandates, not silently swallowed. `written` is
/// carried in the error path only for the debug line.
///
/// # On expiry
///
/// The `RE1_READ_CLOSE_POLL_MS` slice expiring is not an outcome — it is the
/// point at which the registry is re-asked, and the loop continues. There is no
/// second deadline: `java.net.Socket` has no write timeout, and this surface
/// records only a `read_timeout_ms`.
pub fn re1_write_close_aware(sid: i32, stream: &TcpStream, data: &[u8]) -> std::io::Result<usize> {
    let mut written: usize = 0;
    loop {
        if written == data.len() {
            return Ok(written);
        }
        let ready = match re1_socket_poll_writable(stream, RE1_READ_CLOSE_POLL_MS) {
            Some(result) => result?,
            // No poll primitive on this target: the pre-2026-08-12 blocking
            // write, which cannot see the close but at least still transfers.
            None => {
                let mut w = stream;
                w.write_all(&data[written..])?;
                return Ok(data.len());
            }
        };
        // Asked AFTER the poll for the reason `re1_read_close_aware` sets out
        // at length: `re1_close_socket` removes the entry BEFORE it issues the
        // shutdown, so by the time the shutdown's own readiness edge wakes this
        // poll the entry is already gone.
        if !re1_stream_still_registered(sid) {
            return Err(re1_socket_closed_err());
        }
        if !ready {
            continue;
        }
        let end = (written + RE1_WRITE_SLICE_MAX).min(data.len());
        let mut w = stream;
        match w.write(&data[written..end]) {
            Ok(0) => continue,
            Ok(n) => written += n,
            // EINTR has written nothing; retry. Same rationale as
            // `re1_read_retry_eintr`.
            Err(e) if is_eintr(&e) => continue,
            // Writable, then not: a concurrent writer on this socket took the
            // room. Park again rather than report a short write.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
}

/// `owner` is the writing `Socket`'s key; see [`re1_socket_read_stream`].
pub fn re1_socket_write_stream(
    ctx: &mut dyn NativeContext,
    owner: NativeObjKey,
    buf: ObjectRef,
    offset: i32,
    length: i32,
) -> MethodCallResult {
    if length == 0 {
        return Ok(None);
    }
    let data = java_byte_array_to_vec(ctx, buf, offset, length)?;
    // No row clone (gc-common w28-a; see `re1_socket_read_stream`).
    let (stream_id, closed) = sock_peek_key(owner, |s| (s.stream_id, s.closed));
    if stream_id < 0 {
        // Closed by another thread since the stream found its owner row.
        if closed != 0 {
            return Err(re1_socket_exception(ctx, "Socket closed"));
        }
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
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    // ASYNCHRONOUS CLOSE (2026-08-12): a bare `write_all` here is the write
    // twin of the read hole fixed on this surface on 2026-08-11 — a thread
    // parked behind a peer that has stopped reading never noticed
    // `Socket.close()`. `re1_write_close_aware` polls for writability, re-asks
    // the registry, and slices; see its doc comment.
    let write_result = (|| -> std::io::Result<()> {
        re1_write_close_aware(stream_id, &stream, &data)?;
        (&*stream).flush()?;
        Ok(())
    })();
    ctx.end_blocking_region();
    if let Err(e) = write_result {
        // A close observed by the loop above arrives as `Interrupted`, which no
        // other step on this path can produce (`re1_write_close_aware` reissues
        // every real EINTR and the poll reports one as "not ready"). JDK 25's
        // `Socket.close()` specifies that a thread blocked in an I/O operation
        // on the socket throws a `SocketException` — "will", not "may" — and
        // the concrete type is load-bearing because callers catch it above
        // `catch (IOException)`.
        if e.kind() == std::io::ErrorKind::Interrupted {
            return Err(re1_socket_exception(ctx, "Socket closed"));
        }
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
            return Err(cratonvm_native_api::throwable_builder::throw_new(
                ctx,
                "java/net/SocketException",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(jmsg))],
                || ioex(format!("Socket write failed: {e}")),
            ));
        }
        return Err(ioex(format!("Socket write failed: {e}")));
    }
    if crate::nbflags().dbg_sock {
        eprintln!(
            "[dbg-sock] write: sid={stream_id} sent={} bytes",
            data.len()
        );
        if crate::nbflags().dbg_sock_bytes {
            eprintln!(
                "[dbg-sock-bytes] write: sid={stream_id} data={}",
                socket_dbg_bytes(&data)
            );
        }
    }
    Ok(None)
}

pub fn native_socket_input_stream_read_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let owner = re1_stream_owner_or_closed(ctx, this)?;
    let buf = obj_arg(args, 1)?;
    let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
    re1_socket_read_stream(ctx, owner, buf, off, len)
}

pub fn native_socket_input_stream_read_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let owner = re1_stream_owner_or_closed(ctx, this)?;
    let buf = obj_arg(args, 1)?;
    let len = ctx.array_length(buf) as i32;
    re1_socket_read_stream(ctx, owner, buf, 0, len)
}

pub fn native_socket_input_stream_read_one(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let owner = re1_stream_owner_or_closed(ctx, this)?;
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
pub fn re1_init_socket_locks(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    // GC-safety: `this` is a raw ObjectRef parameter, and `new_object` below
    // is a re-entrant, allocating call (it can trigger a GC). This is called
    // right after a fresh Socket allocation -- for a freshly-accepted Socket
    // (see the `ss, "accept"` wrapper), the caller passes this same `this`
    // straight into `re2_accept_into` afterward, so a relocation/reclaim here
    // corrupts the Socket before it is ever returned to Java bytecode.
    // Pin across both allocating iterations, re-reading the current
    // (possibly-forwarded) value before every use.
    let pin_base = ctx.pin_native_root(this);
    if let Some(slot) = ctx.resolve_field_index("java/net/Socket", "socketLock") {
        let cur = ctx.read_native_pin(pin_base, this);
        if ctx.object_num_fields(cur) > slot
            && !matches!(ctx.get_field(cur, slot), Value::Object(Some(_)))
        {
            if let Ok(Some(Value::Object(Some(lock)))) = ctx.new_object("java/lang/Object") {
                let cur = ctx.read_native_pin(pin_base, this);
                ctx.set_field(cur, slot, Value::Object(Some(lock)));
            }
        }
    }
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
    Ok(result)
}

/// Run `f` against the raw `TcpStream` backing socket-state id `sid`,
/// whichever registry table holds it: the plain `streams` map, or a TLS
/// socket's cloned `raw` handle (`tls_streams`). Returns `None` when the
/// socket is not connected or not tracked, so callers can tell "applied" from
/// "nothing to apply it to".
///
/// The TLS arm matters as much as the plain one: `new13_do_create_socket`'s
/// TLS path registers its id in `tls_streams` only, and forgetting that table
/// is exactly the bug `setSoTimeout`/`getSoTimeout` were fixed for above. The
/// `raw` handle is used (never `entry.stream`) so a socket-option call cannot
/// wait on the per-stream TLS mutex a blocked reader may be holding.
///
/// `f` must be a non-blocking operation (a `getsockopt`/`setsockopt`, or a
/// `peek` already known to be satisfiable): the process-wide registry lock is
/// held for its duration.
pub fn re1_with_raw_stream<R>(sid: i32, f: impl FnOnce(&TcpStream) -> R) -> Option<R> {
    if sid >= crate::servlet::RUSTLS_SOCK_ID_BASE {
        return crate::t27_tls::with_raw_tls_stream(sid - crate::servlet::RUSTLS_SOCK_ID_BASE, f);
    }
    if (crate::servlet::PENDING_CONNECT_SOCK_ID_BASE..crate::servlet::PENDING_LAYERED_SOCK_ID_BASE)
        .contains(&sid)
    {
        return crate::phases_late::ssl_security::with_pending_tcp(sid, f);
    }
    if (crate::servlet::PENDING_LAYERED_SOCK_ID_BASE..crate::servlet::RUSTLS_SOCK_ID_BASE)
        .contains(&sid)
    {
        return crate::t27_tls::with_pending_layered_tcp(
            sid - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE,
            f,
        );
    }
    if sid < 0 {
        return None;
    }
    let reg = s2_registry().lock();
    if let Some(stream) = reg.streams.get(&sid) {
        return Some(f(&**stream));
    }
    if let Some(raw) = reg.tls_streams.get(&sid).and_then(|e| e.raw.as_ref()) {
        return Some(f(raw));    }
    None
}

/// Wait up to `timeout_ms` for `stream` to become **readable**.
///
/// `Some(Ok(true))` — readable, or errored/hung up (which the read that
/// follows then surfaces as the concrete socket error); `Some(Ok(false))` —
/// the timeout expired; `Some(Err(_))` — the poll itself failed. `None` means
/// this build has NO poll primitive at all, and is the caller's signal to fall
/// back to a plain blocking read rather than spin on a stub that answers "not
/// ready" forever. **That `None` arm is the one a "simplification" deletes and
/// the one that prevents a livelock**; it is preserved verbatim by
/// `cratonvm_native_io::net::poll_stream_readable`, which is where the three
/// `#[cfg]` arms that used to live here now are.
///
/// COLLAPSED 2026-08-12, W2-2's out-of-file patch. This file used to carry its
/// own `poll(2)`/`WSAPoll` binding — inherited from the zero-timeout readiness
/// probe `SocketInputStream.available()` needs — because the `native-io`
/// primitive both halves of the asynchronous-close wakeup park in
/// (`net.rs::net_read_close_aware`, `socket_channel.rs::read_close_aware`) was
/// `pub` to that crate and unreachable from here. It is now `pub`, along
/// with its write twin, so the duplicate is gone rather than kept in agreement
/// by hand. `native-builtins` already depends on the crate and already calls
/// `cratonvm_native_io::net::take_stream_for_tls`, so no dependency edge is new.
///
/// Three properties survive the collapse unchanged, each of which is a real
/// difference and not noise:
///
/// * the three-state contract above, `None` arm included;
/// * readiness is `rc > 0`, **not** a `revents & POLLIN` mask test —
///   POLLERR/POLLHUP/POLLNVAL are delivered whether or not they were requested
///   and must count as ready, or a reader parks forever on a socket that can
///   never become readable. `net_poll_raw` answers `Ok(count > 0)` for exactly
///   that reason. Do not "restore" a mask test on either side;
/// * EINTR is reported as "not ready", never as an error and never as an
///   in-place re-poll. `poll(2)` is never auto-restarted by `SA_RESTART` and
///   this VM signals parked threads on purpose (`jit::xt_root_scan` SIGUSR2s
///   every thread for a cross-thread root scan), so re-polling in place with the
///   same `timeout_ms` would restart the whole wait on every GC and silently
///   defeat [`re1_read_close_aware`]'s `SO_TIMEOUT` deadline.
///
/// Still outstanding, and deliberately not attempted here: `servlet.rs` and
/// `xnio_conduits.rs` hold the crate's other two bindings. `servlet.rs`'s is
/// `selector_poll` over a `PollReq` **slice**, which the selector needs and this
/// two-argument primitive cannot express — a genuine third shape rather than a
/// fourth copy.
pub fn re1_socket_poll_readable(stream: &TcpStream, timeout_ms: i32) -> Option<std::io::Result<bool>> {
    cratonvm_native_io::net::poll_stream_readable(stream, timeout_ms)
}

/// Wait up to `timeout_ms` for `stream` to become **writable**. Same
/// three-state contract as [`re1_socket_poll_readable`], and the reason the
/// collapse needed the pair exported: this file's own binding had grown a
/// direction parameter, so exporting only the readable half would have forced it
/// to keep a private copy for the write direction — one site of a two-direction
/// idiom converted, which is how a duplicate grows back.
pub fn re1_socket_poll_writable(stream: &TcpStream, timeout_ms: i32) -> Option<std::io::Result<bool>> {
    cratonvm_native_io::net::poll_stream_writable(stream, timeout_ms)
}

/// Zero-timeout OS readability query for a TCP stream.
///
/// Used by `SocketInputStream.available()`, which must never block. A `peek`
/// on a blocking socket with an empty receive queue would block forever, so
/// readiness is established first with `poll(2)` / `WSAPoll` — a pure query of
/// kernel socket state that neither consumes bytes nor flips the socket's
/// persistent blocking mode (flipping it would race a concurrent blocking
/// `read` on the same fd into a spurious `WouldBlock`; see the same rewrite in
/// `native-api/src/fd_table.rs::tcp_available`). A failed probe — and a target
/// with no poll at all — reports not-readable, so `available()` degrades to 0,
/// the answer it gave unconditionally before.
pub fn re1_socket_read_ready(stream: &TcpStream) -> bool {
    matches!(re1_socket_poll_readable(stream, 0), Some(Ok(true)))
}

pub fn re1_connect_socket(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    host: &str,
    port: i32,
    timeout_ms: i32,
) -> MethodCallResult {
    if !(0..=65535).contains(&port) {
        return Err(iae(format!("port out of range: {port}")));
    }
    // gc-common w16-a: close the streams of sockets the last collection found
    // dropped without `close()` (and release queued roots) before opening a
    // new one. Allocates nothing, so `this` stays current.
    net_release_deferred(ctx);
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
        // HotSpot 25.0.4+7: `Connection refused`, and `Connection refused
        // (connect failed)` when the JDK connected non-blocking (a timeout
        // was given) -- `Sbr2W7TailNet.java` `socket.connect*`.
        std::io::ErrorKind::ConnectionRefused => RuntimeError::ConnectException {
            message: if timeout_ms > 0 {
                "Connection refused (connect failed)".to_string()
            } else {
                "Connection refused".to_string()
            },
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
    let read_timeout_ms = sock_peek(ctx, this, |s| s.read_timeout_ms);
    if read_timeout_ms > 0 {
        stream
            .set_read_timeout(Some(Duration::from_millis(read_timeout_ms as u64)))
            .map_err(|e| ioex(format!("apply preconnect SO_TIMEOUT failed: {e}")))?;
    }
    let stream_id = s2_alloc_stream_for_vm(ctx.vm_identity(), stream);
    let pin_base = ctx.pin_native_root(this);
    let this_now = ctx.read_native_pin(pin_base, this);
    sock_set_owned(ctx, this_now, |s| {
        s.host = host.to_string();
        s.port = port;
        s.local_port = local_port;
        s.closed = 0;
        s.stream_id = stream_id;
        // gc-common w16-a: this socket opened the stream, so a `Socket`
        // dropped without `close()` has it closed by the owner-death sweep.
        s.owns_stream = true;
    });
    apply_pending_socket_options(ctx, this_now);
    let this_now = ctx.read_native_pin(pin_base, this);
    let _ = re1_init_socket_locks(ctx, this_now);
    ctx.unpin_native_roots(pin_base);
    Ok(None)
}

pub fn re1_socket_adaptor_inet(
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

    // gc-common w20-d: each accessor below is Java; the address was asked the
    // next one through its address from before the previous. Rooted, read
    // back before each call.
    let mut scope = NativeHandleScope::new(ctx);
    let addr_h = scope.root(socket_addr);
    if let Ok(Some(Value::Object(Some(addr)))) =
        scope.invoke_virtual(socket_addr, "getAddress", "()Ljava/net/InetAddress;", &[])
    {
        return Ok(Some(addr));
    }

    let socket_addr = scope.get(&addr_h);
    let host_string =
        scope.invoke_virtual(socket_addr, "getHostString", "()Ljava/lang/String;", &[]);
    let host = match host_string {
        Ok(Some(Value::Object(Some(s)))) => scope.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let host = if host.is_empty() {
        let socket_addr = scope.get(&addr_h);
        match scope.invoke_virtual(socket_addr, "getHostName", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => scope.read_string(s).unwrap_or_default(),
            _ => String::new(),
        }
    } else {
        host
    };
    drop(scope);
    if host.is_empty() {
        return Ok(None);
    }
    // `getHostString()` hands back a real hostname when the socket address has
    // one and the numeric text otherwise, so route through the input-sensitive
    // allocator rather than assuming either.
    let ip = host.trim_matches(&['[', ']'][..]);
    Ok(Some(alloc_inet_address_for_input(ctx, ip, ip)?))
}

pub fn register_re1_socket(r: &mut NativeMethodRegistry) {
    // NIO-SERVER-SOCKET (route 1): skip the synthetic java.net.Socket surface so
    // real bytecode drives sun/nio/ch/Net. See register_phase53_socket_stubs.
    if crate::vmflags().io.real_net_sockets {
        return;
    }
    register_re1_socket_surface(r);
}

/// The synthetic `java.net.Socket` surface itself, without the
/// `real_net_sockets` gate [`register_re1_socket`] applies. Split out (gc-common
/// w16-a) so unit tests can drive these closures: the gate is ON by default
/// (`CRATONVM_SYNTHETIC_NET_SOCKETS` absent), so a test that went through
/// `register_re1_socket` found nothing registered and failed its `find`.
pub fn register_re1_socket_surface(r: &mut NativeMethodRegistry) {
    let sock = "java/net/Socket";

    r.register(sock, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // The receiver at entry, before anything allocates: current.
        sock_set_owned(ctx, this, |s| {
            s.port = 0;
            s.local_port = 0;
            s.closed = 0;
            s.stream_id = -1;
        });
        let _ = re1_init_socket_locks(ctx, this);
        Ok(None)
    }, NativeKind::SyntheticStub);

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
    }, NativeKind::SyntheticStub);

    r.register(sock, "<init>", "(Ljava/net/InetAddress;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = obj_arg(args, 1)?;
        let host = inet_addr_field_string_or(ctx, addr, IA_ADDR, "127.0.0.1");
        let port = args
            .get(2)
            .and_then(|v| v.as_int())
            .ok_or_else(|| iae("Socket: missing port"))?;
        re1_connect_socket(ctx, this, &host, port, 0)
    }, NativeKind::SyntheticStub);

    r.register(
        sock,
        "connect",
        "(Ljava/net/SocketAddress;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sa = obj_arg(args, 1).map_err(|_| ioex("Socket.connect: null address"))?;
            let (host, port) = read_inet_socket_address(ctx, sa)?;
            re1_connect_socket(ctx, this, &host, port, 0)
        }, NativeKind::SyntheticStub,
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
        }, NativeKind::SyntheticStub,
    );

    r.register(sock, "bind", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(sa))) = args.get(1) {
            let (_, port) = read_inet_socket_address(ctx, *sa)?;
            sock_set(ctx, this, |s| s.local_port = port);        }
        Ok(None)
    }, NativeKind::SyntheticStub);

    // setOption / getOption — the `jdk.net.ExtendedSocketOptions` keepalive
    // family, served HERE because this registrar owns the socket.
    //
    // `native-io`'s `jdk/net/*SocketOptions` natives are reached (proved: the
    // refusal used to carry CratonVM's own message rather than the JDK's) and
    // then cannot resolve the handle id, because a plain `java.net.Socket`'s
    // `TcpStream` lives in `servlet::s2_registry().streams` — a
    // `native-builtins` registry that crate cannot see. Same resolution as
    // `DatagramSocket.setOption`: put the option surface where the socket is.
    //
    // Dispatch is by `SocketOption.name()`, so one arm serves whichever constant
    // object the caller passes. An option this registrar cannot serve is named
    // in the exception rather than silently accepted.
    r.register(
        sock,
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/net/Socket;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            // gc-common w20-d: the value is unboxed BEFORE `name()` (Java);
            // it was read from `args`, which is not remapped, after it.
            let value = args.get(2).copied().unwrap_or(Value::Object(None));
            let raw = ds_unbox_int(ctx, value);
            let name = ds_socket_option_name(ctx, &mut this, args.get(1).copied());
            let Some((level, opt)) = sock_option_level_and_name(&name) else {
                return Err(RuntimeError::UnsupportedOperationException {
                    message: format!("Socket.setOption: {name} is not supported"),
                }
                .into());
            };
            match sock_raw_descriptor(ctx, this) {
                Some(fd) => {
                    sock_set_option_int(fd, level, opt, raw)
                        .map_err(|e| ioex(format!("{name}: {e}")))?;
                }
                None => {
                    // No descriptor yet. On HotSpot that is not an error —
                    // `Socket.getImpl()` creates the impl on demand, so an
                    // option set before `connect` is applied to the socket the
                    // connect then uses. Retain it and replay it there
                    // (`apply_pending_socket_options`). A CLOSED socket is a
                    // genuine error, and keeps saying so.
                    if sock_peek(ctx, this, |s| s.closed) != 0 {
                        return Err(ioex("Socket.setOption: socket is closed"));
                    }
                    sock_set(ctx, this, |s| {
                        s.pending_options
                            .retain(|&(l, o, _)| (l, o) != (level, opt));
                        s.pending_options.push((level, opt, raw));
                    });
                }
            }
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        sock,
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let name = ds_socket_option_name(ctx, &mut this, args.get(1).copied());
            let Some((level, opt)) = sock_option_level_and_name(&name) else {
                return Err(RuntimeError::UnsupportedOperationException {
                    message: format!("Socket.getOption: {name} is not supported"),
                }
                .into());
            };
            // Mirror of `setOption`: before `connect` the answer is whatever
            // was retained for the not-yet-created descriptor.
            let raw = match sock_raw_descriptor(ctx, this) {
                Some(fd) => match sock_get_option_int(fd, level, opt) {
                    Some(raw) => raw,
                    None => return Err(ioex(format!("{name}: option is not readable"))),
                },
                None => {
                    if sock_peek(ctx, this, |s| s.closed) != 0 {
                        return Err(ioex("Socket.getOption: socket is closed"));
                    }
                    match sock_get(ctx, this)
                        .pending_options
                        .iter()
                        .rev()
                        .find(|&&(l, o, _)| (l, o) == (level, opt))
                    {
                        Some(&(_, _, v)) => v,
                        // An unconnected socket that was never told otherwise:
                        // report the option off/zero rather than failing, which
                        // is what reading a fresh fd would have answered.
                        None => 0,
                    }
                }
            };
            if name == "SO_KEEPALIVE" {
                return ctx.invoke(
                    "java/lang/Boolean",
                    "valueOf",
                    "(Z)Ljava/lang/Boolean;",
                    &[Value::Int(i32::from(raw != 0))],
                );
            }
            ds_box_int(ctx, raw)
        }, NativeKind::SyntheticStub,
    );

    r.register(sock, "isConnected", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = sock_get(ctx, this);
        Ok(Some(Value::Int(if s.stream_id >= 0 && s.closed == 0 {
            1
        } else {
            0
        })))
    }, NativeKind::SyntheticStub);
    r.register(sock, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            if sock_peek(ctx, this, |s| s.closed) != 0 {
                1
            } else {
                0
            },
        )))
    }, NativeKind::SyntheticStub);

    r.register(sock, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Minted: `close()` WRITES the row (`isClosed()` answers true after
        // it even for a socket never connected). `this` is the receiver.
        re1_close_socket(mint_native_obj_key(ctx, this))
    }, NativeKind::SyntheticStub);

    r.register(sock, "shutdownInput", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = sock_peek(ctx, this, |s| s.stream_id);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                stream
                    .shutdown(std::net::Shutdown::Read)
                    .map_err(|e| ioex(format!("shutdown read failed: {e}")))?;
            } else {
                // A TLS socket's id lives in `tls_streams`; use its cloned raw
                // handle so this never waits on the per-stream TLS mutex —
                // unblocking a parked reader is the whole point of `shutdown`.
                if let Some(raw) = reg.tls_streams.get(&sid).and_then(|e| e.raw.as_ref()) {
                    raw.shutdown(std::net::Shutdown::Read)
                        .map_err(|e| ioex(format!("shutdown read failed: {e}")))?;
                }
            }
        }
        sock_set(ctx, this, |s| s.input_shutdown = 1);
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(sock, "shutdownOutput", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = sock_peek(ctx, this, |s| s.stream_id);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                stream
                    .shutdown(std::net::Shutdown::Write)
                    .map_err(|e| ioex(format!("shutdown write failed: {e}")))?;
            } else {
                // A TLS socket's id lives in `tls_streams`; use its cloned raw
                // handle so this never waits on the per-stream TLS mutex —
                // unblocking a parked reader is the whole point of `shutdown`.
                if let Some(raw) = reg.tls_streams.get(&sid).and_then(|e| e.raw.as_ref()) {
                    raw.shutdown(std::net::Shutdown::Write)
                        .map_err(|e| ioex(format!("shutdown write failed: {e}")))?;
                }
            }
        }
        sock_set(ctx, this, |s| s.output_shutdown = 1);
        Ok(None)
    }, NativeKind::SyntheticStub);
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
        Ok(Some(Value::Int(sock_peek(ctx, this, |s| s.input_shutdown))))
    }, NativeKind::SyntheticStub);
    r.register(sock, "isOutputShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(sock_peek(ctx, this, |s| {
            s.output_shutdown
        }))))
    }, NativeKind::SyntheticStub);

    r.register(sock, "setSoTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if ms < 0 {
            return Err(iae(format!("negative SO_TIMEOUT: {ms}")));
        }
        let sid = sock_peek(ctx, this, |s| s.stream_id);
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
                if let Some(raw) = entry.raw.as_ref() {
                    raw.set_read_timeout(d)
                        .map_err(|e| ioex(format!("setSoTimeout failed: {e}")))?;
                }
            }
        }
        sock_set(ctx, this, |s| s.read_timeout_ms = ms);
        Ok(None)
    }, NativeKind::SyntheticStub);
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
        let configured = sock_peek(ctx, this, |s| s.read_timeout_ms);
        if configured != 0 {
            return Ok(Some(Value::Int(configured)));
        }
        let sid = sock_peek(ctx, this, |s| s.stream_id);
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
            if let Some(raw) = reg.tls_streams.get(&sid).and_then(|e| e.raw.as_ref()) {
                let ms = raw
                    .read_timeout()
                    .map_err(|e| ioex(format!("getSoTimeout failed: {e}")))?
                    .map(|d| d.as_millis() as i32)
                    .unwrap_or(0);
                return Ok(Some(Value::Int(ms)));
            }
        }
        Ok(Some(Value::Int(0)))
    }, NativeKind::SyntheticStub);

    // The SO_* option GETTERS, for exactly the reason `setSoTimeout`/
    // `getSoTimeout` above were added. Unregistered, they ran real
    // `java.net.Socket` bytecode: `getImpl().getOption(...)`. On a synthetic
    // Socket the real `impl` field is null (this surface keeps its state in
    // the side table and deliberately never writes instance slots), so
    // `getImpl()` takes the `createImpl(true)` branch and manufactures a
    // BRAND NEW OS socket — the answer then described a throwaway fd that no
    // byte of this connection ever passes through, and leaked that fd on the
    // way out. Read the fd this Socket actually owns instead.
    //
    // The matching SETTERS are deliberately NOT registered here: they would be
    // dead code. `native-io/src/socket_channel.rs` blanket-registers
    // `setTcpNoDelay`/`setKeepAlive`/`setReuseAddress`/`set{Send,Receive}
    // BufferSize`/`setSoLinger` on `java/net/Socket` to a constant-`Ok(None)`
    // `socket_opt_noop` (intended only for the `SocketChannel.socket()`
    // adaptor, but keyed on the class so it catches every Socket), and
    // `register_io_natives` runs AFTER `register_essential_natives_with_shims`
    // in `vm/src/vm/vm_init.rs` — so last-writer-wins gives those no-ops the
    // slot. Until that registration is narrowed, these getters report the
    // socket's TRUE state, which is precisely "the option was never applied".
    // `setSoTimeout` is unaffected: socket_channel.rs deliberately excludes it.
    r.register(sock, "getTcpNoDelay", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = sock_peek(ctx, this, |s| s.stream_id);
        let on = re1_with_raw_stream(sid, |s| s.nodelay().unwrap_or(false)).unwrap_or(false);
        Ok(Some(Value::Int(if on { 1 } else { 0 })))
    }, NativeKind::SyntheticStub);
    r.register(sock, "getKeepAlive", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = sock_peek(ctx, this, |s| s.stream_id);
        let on = re1_with_raw_stream(sid, |s| {
            socket2::SockRef::from(s).keepalive().unwrap_or(false)
        })
        .unwrap_or(false);
        Ok(Some(Value::Int(if on { 1 } else { 0 })))
    }, NativeKind::SyntheticStub);
    r.register(sock, "getReuseAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = sock_peek(ctx, this, |s| s.stream_id);
        let on = re1_with_raw_stream(sid, |s| {
            socket2::SockRef::from(s).reuse_address().unwrap_or(false)
        })
        .unwrap_or(false);
        Ok(Some(Value::Int(if on { 1 } else { 0 })))
    }, NativeKind::SyntheticStub);
    r.register(sock, "getSendBufferSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = sock_peek(ctx, this, |s| s.stream_id);
        // 8192 only when the socket cannot answer (unconnected): the same
        // fallback the pre-existing phase-53 surface used.
        let sz = re1_with_raw_stream(sid, |s| {
            socket2::SockRef::from(s).send_buffer_size().unwrap_or(8192)
        })
        .unwrap_or(8192);
        Ok(Some(Value::Int(sz as i32)))
    }, NativeKind::SyntheticStub);
    r.register(sock, "getReceiveBufferSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = sock_peek(ctx, this, |s| s.stream_id);
        let sz = re1_with_raw_stream(sid, |s| {
            socket2::SockRef::from(s).recv_buffer_size().unwrap_or(8192)
        })
        .unwrap_or(8192);
        Ok(Some(Value::Int(sz as i32)))
    }, NativeKind::SyntheticStub);
    r.register(sock, "getSoLinger", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = sock_peek(ctx, this, |s| s.stream_id);
        // -1 is the spec'd "SO_LINGER disabled" answer, and also the honest
        // answer for a socket with no fd to ask.
        let secs = re1_with_raw_stream(sid, |s| match socket2::SockRef::from(s).linger() {
            Ok(Some(d)) => d.as_secs() as i32,
            _ => -1,
        })
        .unwrap_or(-1);
        Ok(Some(Value::Int(secs)))
    }, NativeKind::SyntheticStub);

    r.register(sock, "getPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(sock_peek(ctx, this, |s| s.port))))
    }, NativeKind::SyntheticStub);
    r.register(sock, "getLocalPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(sock_peek(ctx, this, |s| s.local_port))))
    }, NativeKind::SyntheticStub);
    r.register(
        sock,
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // gc-common w20-d: the side-table row is read BEFORE the adaptor
            // probe, which runs Java; its key is derived from `this`, which
            // was read through its pre-call address.
            let host = sock_get(ctx, this).host;
            if let Some(addr) = re1_socket_adaptor_inet(ctx, this, false)? {
                return Ok(Some(Value::Object(Some(addr))));
            }
            if host.is_empty() {
                return Ok(Some(Value::Object(None)));
            }
            let ip = resolve_host(&host)
                .map(|i| i.to_string())
                .unwrap_or_else(|_| host.clone());
            let ia = alloc_inet_address(ctx, &host, &ip)?;
            Ok(Some(Value::Object(Some(ia))))
        }, NativeKind::SyntheticStub,
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
    // netty-client-socket-write-after-close-nsme-FIXED.md).
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
            let ia = alloc_inet_address(ctx, "localhost", "127.0.0.1")?;
            Ok(Some(Value::Object(Some(ia))))
        }, NativeKind::SyntheticStub,
    );

    r.register(
        sock,
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sid = sock_peek(ctx, this, |s| s.stream_id);
            if sid < 0 {
                return Err(ioex("Socket.getInputStream: not connected"));
            }
            // A layered SSLSocket can be invoked through its java.net.Socket
            // base type (MockWebServer does exactly this). Keep that virtual
            // call on the rustls-aware stream adapter instead of treating its
            // high-offset id as a plain raw s2 socket id.
            if sid >= crate::servlet::RUSTLS_SOCK_ID_BASE {
                let is =
                    try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketInputStream", 1)?;
                // gc-common w16-a: owned (`is` is fresh), so the row goes with
                // the stream object. Every call mints a stream, and each used
                // to leave its row until VM teardown. The TLS id stays the
                // socket's: `owns_stream` is false.
                sock_set_owned(ctx, is, |s| {
                    s.port = 0;
                    s.local_port = 0;
                    s.closed = 0;
                    s.stream_id = sid;
                    s.owns_stream = false;
                });
                return Ok(Some(Value::Object(Some(is))));
            }
            // gc-common w15-a: the stream remembers its socket by KEY, taken
            // before the allocation (a lock key follows its object across a
            // move, gc-common w28-a), so nothing needs pinning and nothing is
            // rooted. It used to take a global root on the socket that nothing
            // ever released.
            let owner = mint_native_obj_key(&*ctx, this);
            let is = try_alloc_concurrent_synthetic(ctx, "java/net/Socket$SocketInputStream", 3)?;
            // Side-table the stream's owner so we don't depend on field
            // layout (real `Socket$SocketInputStream` has different fields
            // than the synthetic shape: `parent:Socket`, `in:InputStream`).
            // gc-common w16-a: with the fresh stream's address, which keeps
            // the socket's rows (and its connection) while the stream lives.
            let is_key = mint_native_obj_key(&*ctx, is);
            stream_owner_set_at(is_key, owner, is.as_ptr() as usize);
            Ok(Some(Value::Object(Some(is))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        sock,
        "getOutputStream",
        "()Ljava/io/OutputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sid = sock_peek(ctx, this, |s| s.stream_id);
            if sid < 0 {
                return Err(ioex("Socket.getOutputStream: not connected"));
            }
            if sid >= crate::servlet::RUSTLS_SOCK_ID_BASE {
                let os =
                    try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketOutputStream", 1)?;
                // gc-common w16-a: owned, as in `getInputStream` above.
                sock_set_owned(ctx, os, |s| {
                    s.port = 0;
                    s.local_port = 0;
                    s.closed = 0;
                    s.stream_id = sid;
                    s.owns_stream = false;
                });
                return Ok(Some(Value::Object(Some(os))));
            }
            // gc-common w15-a: by key, as in `getInputStream` above.
            let owner = mint_native_obj_key(&*ctx, this);
            let os = try_alloc_concurrent_synthetic(ctx, "java/net/Socket$SocketOutputStream", 3)?;
            let os_key = mint_native_obj_key(&*ctx, os);
            stream_owner_set_at(os_key, owner, os.as_ptr() as usize);
            Ok(Some(Value::Object(Some(os))))
        }, NativeKind::SyntheticStub,
    );

    let sis = "java/net/Socket$SocketInputStream";
    r.register(
        sis,
        "read",
        "([BII)I",
        native_socket_input_stream_read_bytes, NativeKind::SyntheticStub,
    );
    // read([B)I — MUST be registered directly. Without it, `in.read(byte[])`
    // falls to the default java.io.InputStream.read(byte[]) bytecode, which
    // reads ONE byte then loops single-byte read() to fill the ENTIRE array,
    // blocking forever after the first record (e.g. it gets a 5-byte "PING\n"
    // into a 64-byte buffer then blocks waiting for byte 6 that never comes).
    // A single bulk read returning whatever is currently available (>=1 byte)
    // is the correct InputStream.read(byte[]) contract and unblocks every
    // server-side request read (okhttp MockWebServer, loopback HTTP). BUG-04.
    r.register(sis, "read", "([B)I", native_socket_input_stream_read_array, NativeKind::SyntheticStub);
    r.register(sis, "read", "()I", native_socket_input_stream_read_one, NativeKind::SyntheticStub);
    cratonvm_native_api::socket_input_stream_read::set_read_bytes(
        native_socket_input_stream_read_bytes,
    );
    cratonvm_native_api::socket_input_stream_read::set_read_array(
        native_socket_input_stream_read_array,
    );
    cratonvm_native_api::socket_input_stream_read::set_read_one(
        native_socket_input_stream_read_one,
    );
    // Closing either socket stream closes the SOCKET — that is the documented
    // contract of `Socket.getInputStream()`/`getOutputStream()` ("Closing the
    // returned stream will close the associated socket"), and callers rely on
    // it: code that wraps the stream in a `BufferedReader`/`PrintWriter` and
    // closes only the wrapper (directly, or via try-with-resources) expects
    // the connection to go away. As a no-op it did not, so the peer stayed
    // parked in a blocking read waiting for a FIN that never arrived and the
    // OS fd leaked for the process lifetime.
    r.register(sis, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match stream_owner_get(ctx, this) {
            Some(owner) => re1_close_socket(owner),
            // No owner recorded (stream handed out before the side table was
            // populated) — nothing to close, and throwing here would be worse
            // than the historical no-op.
            None => Ok(None),
        }
    }, NativeKind::SyntheticStub);
    r.register(sis, "available", "()I", |ctx, args| {
        // `available()` must report bytes readable WITHOUT blocking. The old
        // constant 0 said "nothing buffered" even on a socket with a full
        // receive queue, so every `while (in.available() > 0) …` drain loop
        // exited immediately and every `if (available() > 0)` fast path was
        // dead — a silent truncation, not a hang.
        let this = obj_arg(args, 0)?;
        let Some(owner) = stream_owner_get(ctx, this) else {
            return Ok(Some(Value::Int(0)));
        };
        let sid = sock_peek_key(owner, |s| s.stream_id);
        // A TLS stream id answers about CIPHERTEXT, not the plaintext this
        // stream hands out: a readable raw socket may hold nothing but a
        // handshake or alert record, so a non-zero answer here could send a
        // caller into a `read()` that then blocks. Report 0 for TLS — the
        // conservative direction, and no worse than the previous constant.
        if sid >= crate::servlet::RUSTLS_SOCK_ID_BASE {
            return Ok(Some(Value::Int(0)));
        }
        let avail = re1_with_raw_stream(sid, |stream| {
            if !re1_socket_read_ready(stream) {
                return 0i32;
            }
            // The kernel says readable, so this `peek` returns immediately
            // and does not consume the bytes. Capped at the scratch buffer:
            // under-reporting is permitted by the `available()` contract,
            // over-reporting is not.
            let mut buf = [0u8; 8192];
            match stream.peek(&mut buf) {
                Ok(n) => n as i32,
                // Readiness raced away (a concurrent reader drained the
                // queue). A snapshot estimate of 0 is correct again.
                Err(_) => 0,
            }
        })
        .unwrap_or(0);
        Ok(Some(Value::Int(avail)))
    }, NativeKind::SyntheticStub);

    let sos = "java/net/Socket$SocketOutputStream";
    r.register(sos, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner = re1_stream_owner_or_closed(ctx, this)?;
        let buf = obj_arg(args, 1)?;
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        re1_socket_write_stream(ctx, owner, buf, off, len)
    }, NativeKind::SyntheticStub);
    r.register(sos, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner = re1_stream_owner_or_closed(ctx, this)?;
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) & 0xff;
        let one = ctx.new_array(ArrayElementType::Byte, 1);
        ctx.set_array_element(one, 0, Value::Int(b as i8 as i32));
        re1_socket_write_stream(ctx, owner, one, 0, 1)
    }, NativeKind::SyntheticStub);
    r.register(sos, "flush", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(owner) = stream_owner_get(ctx, this) {
            let sid = sock_peek_key(owner, |s| s.stream_id);
            if sid >= 0 {
                let mut reg = s2_registry().lock();
                if let Some(stream) = reg.streams.get_mut(&sid) {
                    (&**stream)
                        .flush()
                        .map_err(|e| ioex(format!("flush failed: {e}")))?;                }            }        }
        Ok(None)
    }, NativeKind::SyntheticStub);
    // See `sis, "close"` above — closing the output stream closes the socket
    // too, and does so AFTER flushing whatever the peer has not been sent yet
    // (real `SocketOutputStream.close()` flushes first). Without the flush a
    // caller that writes-then-closes could lose its last write.
    r.register(sos, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let Some(owner) = stream_owner_get(ctx, this) else {
            return Ok(None);
        };
        let sid = sock_peek_key(owner, |s| s.stream_id);
        if sid >= 0 {
            let mut reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get_mut(&sid) {
                // A failed flush must not abort the close: the fd still has
                // to be released, and `close()` is frequently called from a
                // `finally` where a throw would mask the real error.
                let _ = (&**stream).flush();
            }
        }
        re1_close_socket(owner)
    }, NativeKind::SyntheticStub);
}

/// Shut down and forget the TCP stream backing `this`, and mark the socket
/// closed in the side table. Shared by `Socket.close()` and by the two socket
/// stream `close()` natives, which are specified to close the socket as well.
/// Idempotent: closing an already-closed socket is a no-op, per `Socket`.
///
/// Takes the socket's key, not the object (gc-common w15-a): the streams know
/// their socket only by key. Also drops the socket's stream -> owner rows; a
/// read or write through one of its streams now answers `Socket closed`.
pub fn re1_close_socket(this: NativeObjKey) -> MethodCallResult {
    let sid = sock_peek_key(this, |s| s.stream_id);
    if sid >= 0 {
        let mut reg = s2_registry().lock();
        if let Some(stream) = reg.streams.remove(&sid) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
    sock_set_key(this, |s| {
        s.stream_id = -1;
        s.closed = 1;
        s.owns_stream = false;
    });
    stream_owners_forget_owner(this);
    Ok(None)
}

