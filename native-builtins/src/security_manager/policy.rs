// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Parser for `java.policy` files.
//!
//! Grammar supported (simplified JDK 8+ policy file syntax):
//!
//! ```text
//! grant [codeBase "URL"] {
//!     permission PermClass ["target"] [, "actions"];
//!     permission PermClass;
//!     ...
//! };
//! ```
//!
//! - `//` line comments and `/* ... */` block comments are stripped before
//!   lexical analysis.
//! - String literals are double-quoted; escape sequences `\"`, `\\`, `\n`,
//!   `\r`, `\t` are honored.
//! - Multiple `grant` blocks per file are allowed.
//! - `signedBy` and `principal` are tolerated but not currently matched
//!   against — they are parsed and preserved as metadata so a future
//!   extension can consult them.
//!
//! The parser is hand-rolled (zero external dependencies) and produces a
//! `Policy` whose `implies(perm, target, actions, code_base)` method returns
//! true if any grant covers the requested permission.

use std::borrow::Cow;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use super::x509;

// ---------------------------------------------------------------------------
// Public data model
// ---------------------------------------------------------------------------

/// A parsed java.policy file. A policy contains zero or more `Grant`
/// blocks; a permission is granted if any grant implies it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    pub grants: Vec<Grant>,
}

/// One `grant { ... };` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grant {
    /// Optional `codeBase "URL"` filter. `None` means the grant applies to
    /// every code source.
    pub code_base: Option<String>,
    /// Optional `signedBy "alias"` filter. Preserved but not currently
    /// enforced (we have no keystore to resolve aliases against).
    pub signed_by: Option<String>,
    /// `principal <class> "name"` clauses (zero or more — JDK supports
    /// AND-style multi-principal grants).  When this list is non-empty
    /// the grant ONLY applies to subjects carrying ALL of the listed
    /// principals; an unauthenticated context never matches.  The
    /// previous version of the parser silently dropped these clauses,
    /// turning a restricted grant into a wildcard grant — a security-
    /// hostile behaviour now fixed in WP6.8.
    pub principals: Vec<PrincipalEntry>,
    /// WP6.8 — set when this grant came from a `${...}` substitution
    /// that failed to resolve.  The JDK PolicyParser drops such grants
    /// entirely (so they cannot accidentally over-permit) instead of
    /// emitting an unsubstituted literal that would never match the
    /// canonical form anyway.  We mirror that semantics: a grant with
    /// `disabled == true` is skipped at `implies` time.
    pub disabled: bool,
    /// Permissions in this grant block.
    pub permissions: Vec<PermissionEntry>,
}

/// One `permission ClassName "target", "actions";` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionEntry {
    /// The permission class (slash or dot form), e.g. `java.io.FilePermission`.
    pub class_name: String,
    /// The target/name (first quoted arg), e.g. `"/tmp/*"`.
    pub target: Option<String>,
    /// The actions (second quoted arg), e.g. `"read,write"`.
    pub actions: Option<String>,
}

/// WP6.8 — a `principal <class> "name"` filter parsed from a grant
/// header.  The JDK supports a star-wildcard in either field
/// (`principal * *`, `principal X500Principal *`, etc.) and matches
/// case-sensitively against the calling subject's principal set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrincipalEntry {
    /// Fully-qualified Principal class name (or `*` for wildcard).
    pub class_name: String,
    /// The principal's name string (or `*` for wildcard).
    pub name: String,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum PolicyError {
    Io(std::io::Error),
    /// Syntax/semantic error; `line` is 1-based.
    Parse {
        line: usize,
        message: String,
    },
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::Io(e) => write!(f, "policy I/O error: {e}"),
            PolicyError::Parse { line, message } => {
                write!(f, "policy parse error at line {line}: {message}")
            }
        }
    }
}

impl std::error::Error for PolicyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PolicyError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PolicyError {
    fn from(e: std::io::Error) -> Self {
        PolicyError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Policy API
// ---------------------------------------------------------------------------

impl Policy {
    /// Parse a policy from raw text.
    pub fn parse(source: &str) -> Result<Self, PolicyError> {
        Parser::new(source).parse_policy()
    }

    /// Parse a policy from a file on disk.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, PolicyError> {
        let text = fs::read_to_string(path.as_ref())?;
        Self::parse(&text)
    }

    /// Number of grants.
    pub fn len(&self) -> usize {
        self.grants.len()
    }

    /// True if no grants are defined.
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    /// Does any grant in this policy imply the requested permission?
    ///
    /// - `permission_class`: slash or dot form (`java/io/FilePermission`
    ///   and `java.io.FilePermission` are both accepted).
    /// - `target`: the permission's name/target (e.g. a file path).
    /// - `actions`: comma-separated action list (e.g. `"read,write"`).
    /// - `code_base`: the code source URL of the currently-executing frame
    ///   (see `security_manager::current_privileged_code_base()`).
    pub fn implies(
        &self,
        permission_class: &str,
        target: &str,
        actions: &str,
        code_base: Option<&str>,
    ) -> bool {
        self.implies_full::<String>(permission_class, target, actions, code_base, &[])
    }

    /// Same as [`Policy::implies`] but also filters grants by the calling
    /// frame's signer certificate SHA-256 digests.  A grant whose
    /// `signedBy` alias isn't present in `cert_digests` is skipped
    /// entirely — the calling code must produce a matching digest to
    /// claim the grant.
    pub fn implies_full<S: AsRef<str>>(
        &self,
        permission_class: &str,
        target: &str,
        actions: &str,
        code_base: Option<&str>,
        cert_digests: &[S],
    ) -> bool {
        self.implies_full_with_principals(
            permission_class,
            target,
            actions,
            code_base,
            cert_digests,
            &[],
        )
    }

    /// WP6.8 — full implication check with explicit principal context.
    /// `subject_principals` is a slice of `(class_name, name)` pairs
    /// representing the principals carried by the calling Subject; an
    /// empty slice means "unauthenticated".  Grants with one or more
    /// `principal` clauses ONLY match when every clause is satisfied
    /// by some entry in `subject_principals` — this fixes the previous
    /// silent-downgrade where principal-restricted grants behaved as
    /// unrestricted wildcards.
    pub fn implies_full_with_principals<S: AsRef<str>>(
        &self,
        permission_class: &str,
        target: &str,
        actions: &str,
        code_base: Option<&str>,
        cert_digests: &[S],
        subject_principals: &[(String, String)],
    ) -> bool {
        let norm_class = normalize_class(permission_class);
        let norm_class_ref: &str = norm_class.as_ref();
        for grant in &self.grants {
            // WP6.8: a grant whose `${...}` substitution failed must
            // never imply anything.  This is the "no silent downgrade"
            // anchor — better to skip than to leave an
            // unsubstituted-literal pattern that fails to match in
            // confusing ways.
            if grant.disabled {
                continue;
            }
            if !code_base_matches(grant.code_base.as_deref(), code_base) {
                continue;
            }
            if !signed_by_matches(grant.signed_by.as_deref(), cert_digests) {
                continue;
            }
            if !principals_match(&grant.principals, subject_principals) {
                continue;
            }
            for perm in &grant.permissions {
                // AllPermission implies everything — short-circuit class /
                // target / action matching entirely.
                if normalize_class(&perm.class_name).as_ref() == "java.security.AllPermission" {
                    return true;
                }
                if !class_matches(&perm.class_name, norm_class_ref) {
                    continue;
                }
                if !permission_target_matches(&perm.class_name, perm.target.as_deref(), target) {
                    continue;
                }
                if !actions_match(perm.actions.as_deref(), actions) {
                    continue;
                }
                return true;
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Implication helpers
// ---------------------------------------------------------------------------

/// Convert a permission class name to dotted form, returning a `Cow<str>`
/// that borrows the input when normalization is a no-op (no '/' present).
/// On the `Policy::implies_full` hot path the request's permission class is
/// already dotted (`java.io.FilePermission`), so this avoids the per-call
/// `String` allocation the previous `s.replace('/', ".")` always paid.
fn normalize_class(s: &str) -> Cow<'_, str> {
    if s.contains('/') {
        Cow::Owned(s.replace('/', "."))
    } else {
        Cow::Borrowed(s)
    }
}

fn class_matches(grant_class: &str, request_class: &str) -> bool {
    // java.security.AllPermission always matches.
    if grant_class == "java.security.AllPermission" {
        return true;
    }
    normalize_class(grant_class).as_ref() == request_class
}

/// Match the `codeBase` filter. An unconstrained grant (`None`) matches
/// anything. A constrained grant matches either:
/// - an exact URL prefix (for `file:` URIs), or
/// - the `class:java/lang/Foo` synthetic form used by our
///   `AccessController.doPrivileged` wrapper when no real protection
///   domain is available.
fn code_base_matches(grant_cb: Option<&str>, request_cb: Option<&str>) -> bool {
    let Some(pattern) = grant_cb else {
        return true;
    };
    let Some(actual) = request_cb else {
        // If the grant is constrained but no code base is in scope, the
        // grant does not apply.
        return false;
    };
    // JDK-style wildcard suffixes: a trailing `/*` matches one level; a
    // trailing `/-` matches recursively. Everything else is a prefix match.
    if let Some(stem) = pattern.strip_suffix("/-") {
        actual.starts_with(stem)
    } else if let Some(stem) = pattern.strip_suffix("/*") {
        // `"foo/*"` matches `"foo/X"` where X has no further `/`.
        if let Some(rest) = actual.strip_prefix(stem).and_then(|r| r.strip_prefix('/')) {
            !rest.contains('/')
        } else {
            false
        }
    } else {
        actual == pattern
    }
}

fn target_matches(grant_target: Option<&str>, request_target: &str) -> bool {
    let Some(pattern) = grant_target else {
        // An absent target (e.g. `permission RuntimePermission;`) only
        // matches an empty request target. For the common case of
        // `RuntimePermission "createClassLoader"` we still want an exact
        // match against the request target.
        return request_target.is_empty();
    };
    // Wildcards:
    //   "*"       → any target
    //   "foo/*"   → one more path component
    //   "foo/-"   → any descendants
    //   "*:80"    → host wildcard for SocketPermission
    if pattern == "*" || pattern == "<<ALL FILES>>" {
        return true;
    }
    if let Some(stem) = pattern.strip_suffix("/-") {
        return request_target.starts_with(stem);
    }
    if let Some(stem) = pattern.strip_suffix("/*") {
        if let Some(rest) = request_target
            .strip_prefix(stem)
            .and_then(|r| r.strip_prefix('/'))
        {
            return !rest.contains('/');
        }
        return false;
    }
    // SocketPermission-style "*:<port>" pattern.
    if let Some(rest) = pattern.strip_prefix("*:") {
        if let Some(req_port) = request_target.rsplit(':').next() {
            return rest == req_port;
        }
    }
    pattern == request_target
}

/// Dispatch target matching by permission class — SocketPermission gets
/// an expanded grammar (CIDR blocks, port ranges, domain wildcards);
/// everything else uses the generic `target_matches`.
fn permission_target_matches(
    class_name: &str,
    grant_target: Option<&str>,
    request_target: &str,
) -> bool {
    if normalize_class(class_name).as_ref() == "java.net.SocketPermission" {
        return socket_permission_matches(grant_target, request_target);
    }
    target_matches(grant_target, request_target)
}

/// SocketPermission target matching (JDK SocketPermission.implies semantics,
/// simplified):
///
/// * `"*:port"` — host wildcard for a fixed port.
/// * `"hostname:port"` — exact host + port.
/// * `"hostname:port1-port2"` — port range.
/// * `"*.example.com:port"` — right-anchored domain wildcard.
/// * `"10.0.0.0/8:port"` / `"192.168.0.0/16"` — IPv4 CIDR blocks.
/// * `"localhost"` — alias for 127.0.0.1 + ::1 (and the bare name).
/// * A missing port in the pattern means "any port".
///
/// Both sides use the `host[:port]` format. When the request has no
/// port, it is treated as wildcard; ditto for the grant pattern.
fn socket_permission_matches(grant_target: Option<&str>, request_target: &str) -> bool {
    let Some(pattern) = grant_target else {
        return request_target.is_empty();
    };
    if pattern == "*" || pattern == "<<ALL FILES>>" {
        return true;
    }

    let (pat_host, pat_port) = split_socket_target(pattern);
    let (req_host, req_port) = split_socket_target(request_target);

    if !socket_port_matches(pat_port, req_port) {
        return false;
    }
    socket_host_matches(pat_host, req_host)
}

/// Split a SocketPermission target into `(host, port_spec)`.  `port_spec`
/// is the raw port or port-range substring (e.g. `"80"`, `"8080-8090"`,
/// `""`) — an empty string means "any port".
///
/// Accepts four syntactic forms:
/// * `[<ipv6>]:<port>`   — bracketed IPv6 with port.
/// * `[<ipv6>]`          — bracketed IPv6 without port.
/// * `<ipv6>/<prefix>`   — bare IPv6 CIDR (T17.Γ.2; no port allowed).
/// * `<ipv6>`            — bare IPv6 address (zero-compression allowed).
/// * `<host>:<port>`     — IPv4 / hostname with port (classic form).
/// * `<host>`            — IPv4 / hostname without port.
fn split_socket_target(target: &str) -> (&str, &str) {
    // IPv6 literals use `[addr]:port`.  Pull the bracket body out first.
    if let Some(stripped) = target.strip_prefix('[') {
        if let Some(end) = stripped.find(']') {
            let host = &stripped[..end];
            let rest = &stripped[end + 1..];
            let port = rest.strip_prefix(':').unwrap_or("");
            return (host, port);
        }
    }
    // Bare IPv6 CIDR: the `/` terminates the host portion and leaves no
    // port.  `::/0`, `fe80::/10`, `::1/128`.
    if target.contains('/') && looks_like_bare_ipv6_cidr(target) {
        return (target, "");
    }
    // For ambiguous strings like `::1:8080` we stick with the legacy
    // split-on-last-colon behaviour so the existing T11 localhost alias
    // test (grant "localhost:8080" ⇒ request "::1:8080") keeps matching.
    // Users who really want a bare IPv6 literal with no port should use
    // the bracket-free form `[::1]` (handled above) or the dotted-form
    // `[::1]:*`.
    match target.rsplit_once(':') {
        Some((host, port)) => (host, port),
        None => (target, ""),
    }
}

/// Cheap heuristic — `looks_like_bare_ipv6_cidr("fe80::/10")` is true.
/// We split on the last `/`, then check the prefix parses as IPv6.
fn looks_like_bare_ipv6_cidr(s: &str) -> bool {
    let Some((addr, _prefix)) = s.rsplit_once('/') else {
        return false;
    };
    addr.parse::<Ipv6Addr>().is_ok()
}

/// Match a port or port-range pattern against a concrete port.  Empty
/// pattern or empty request is treated as wildcard match.
fn socket_port_matches(pat_port: &str, req_port: &str) -> bool {
    if pat_port.is_empty() || req_port.is_empty() {
        return true;
    }
    let req: u32 = match req_port.parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    // Pattern can be a single port or a range `lo-hi`.
    if let Some((lo, hi)) = pat_port.split_once('-') {
        let lo_n: u32 = if lo.is_empty() {
            0
        } else {
            lo.parse().unwrap_or(u32::MAX)
        };
        let hi_n: u32 = if hi.is_empty() {
            65535
        } else {
            hi.parse().unwrap_or(0)
        };
        return lo_n <= req && req <= hi_n;
    }
    match pat_port.parse::<u32>() {
        Ok(n) => n == req,
        Err(_) => false,
    }
}

/// Match a host / hostname / wildcard-domain / CIDR pattern against the
/// request host.  Handles the `localhost` alias transparently on both
/// sides and supports both IPv4 and IPv6 CIDR ranges (T17.Γ.2).
fn socket_host_matches(pattern: &str, request: &str) -> bool {
    // Normalize localhost aliases on both sides.
    let pattern = pattern.trim();
    let request = request.trim();

    if pattern == "*" {
        return true;
    }

    // CIDR: `<addr>/<prefix>` for IPv4 or IPv6.
    if let Some((cidr_addr, cidr_prefix)) = pattern.split_once('/') {
        // Resolve request — strip optional IPv6 brackets and translate
        // `localhost` → a numeric address we can parse.
        let req_str = strip_ipv6_brackets(request);
        let req_str = resolve_localhost_multi(req_str);
        if let Ok(prefix) = cidr_prefix.parse::<u32>() {
            // Attempt IPv4 first.
            if let Some(base4) = parse_ipv4(cidr_addr) {
                if let Some(req_ip4) = parse_ipv4(&req_str) {
                    return ipv4_cidr_contains(base4, prefix, req_ip4);
                }
                // IPv4 pattern but non-IPv4 request → no match.
                return false;
            }
            // Fall through to IPv6.
            let base6_str = strip_ipv6_brackets(cidr_addr);
            if let Ok(base6) = base6_str.parse::<Ipv6Addr>() {
                if let Ok(req_ip6) = req_str.parse::<Ipv6Addr>() {
                    return ipv6_cidr_contains(base6, prefix, req_ip6);
                }
                // IPv4-mapped request vs IPv6 pattern: map the request
                // into v4-mapped IPv6 space before comparing.
                if let Some(req_ip4) = parse_ipv4(&req_str) {
                    let mapped = ipv4_to_mapped_v6(req_ip4);
                    return ipv6_cidr_contains(base6, prefix, mapped);
                }
                return false;
            }
        }
        return false;
    }

    // Right-anchored domain wildcard: `*.example.com`.
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return request.eq_ignore_ascii_case(suffix)
            || request
                .to_ascii_lowercase()
                .ends_with(&format!(".{}", suffix.to_ascii_lowercase()));
    }

    // Localhost alias: grant `localhost` implies 127.0.0.1 / ::1 / localhost.
    if pattern.eq_ignore_ascii_case("localhost") {
        let req_str = strip_ipv6_brackets(request);
        return req_str.eq_ignore_ascii_case("localhost")
            || req_str == "127.0.0.1"
            || req_str == "::1"
            || req_str
                .parse::<Ipv6Addr>()
                .map(|a| a == Ipv6Addr::LOCALHOST)
                .unwrap_or(false);
    }

    // Raw IPv6 literal match: accept both bracketed and unbracketed forms.
    let pat_stripped = strip_ipv6_brackets(pattern);
    let req_stripped = strip_ipv6_brackets(request);
    if let (Ok(pa), Ok(pb)) = (
        pat_stripped.parse::<Ipv6Addr>(),
        req_stripped.parse::<Ipv6Addr>(),
    ) {
        return pa == pb;
    }

    pattern.eq_ignore_ascii_case(request)
}

/// Strip `[...]` brackets from an IPv6 literal.  Returns the input
/// unchanged if no brackets are present.
fn strip_ipv6_brackets(s: &str) -> &str {
    if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        inner
    } else {
        s
    }
}

/// Translate `localhost` to `127.0.0.1` **as a String** so we can also
/// feed it to the IPv6 parser if needed (caller decides which).
fn resolve_localhost_multi(s: &str) -> String {
    if s.eq_ignore_ascii_case("localhost") {
        "127.0.0.1".to_string()
    } else {
        s.to_string()
    }
}

/// Build a v4-mapped IPv6 address from an IPv4 u32.  Per RFC 4291 §2.5.5.2
/// the mapping is `::ffff:a.b.c.d`.
fn ipv4_to_mapped_v6(v4: u32) -> Ipv6Addr {
    let octets = v4.to_be_bytes();
    Ipv6Addr::new(
        0,
        0,
        0,
        0,
        0,
        0xffff,
        ((octets[0] as u16) << 8) | (octets[1] as u16),
        ((octets[2] as u16) << 8) | (octets[3] as u16),
    )
}

/// IPv6 CIDR containment check.  `prefix` of 0 matches everything; a
/// prefix > 128 always fails.
fn ipv6_cidr_contains(base: Ipv6Addr, prefix: u32, addr: Ipv6Addr) -> bool {
    if prefix > 128 {
        return false;
    }
    if prefix == 0 {
        return true;
    }
    let base_bits = u128::from_be_bytes(base.octets());
    let addr_bits = u128::from_be_bytes(addr.octets());
    let mask = if prefix == 128 {
        u128::MAX
    } else {
        !((1u128 << (128 - prefix)) - 1)
    };
    (base_bits & mask) == (addr_bits & mask)
}

/// Parse a dotted-decimal IPv4 address into a u32. Returns `None` on
/// malformed input.
fn parse_ipv4(s: &str) -> Option<u32> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut out: u32 = 0;
    for part in parts {
        let byte: u8 = part.parse().ok()?;
        out = (out << 8) | (byte as u32);
    }
    Some(out)
}

/// IPv4 CIDR containment check: does `base/prefix` include `addr`?
fn ipv4_cidr_contains(base: u32, prefix: u32, addr: u32) -> bool {
    if prefix > 32 {
        return false;
    }
    if prefix == 0 {
        return true;
    }
    let mask = if prefix == 32 {
        u32::MAX
    } else {
        !((1u32 << (32 - prefix)) - 1)
    };
    (base & mask) == (addr & mask)
}

/// `signedBy` filter: decide whether a grant's `signedBy "<alias>"`
/// clause is satisfied by the calling frame's signer information.
///
/// `cert_digests` is a mixed slice containing:
/// * SHA-256 hex digests (64 lowercase hex chars) of PKCS#7 signer
///   blocks — Delta's original mode;
/// * Short legacy alias strings used by existing policy tests
///   (e.g. `"acme"`, `"abc123"`); and
/// * Canonical RFC 4514 DN strings returned by
///   [`super::x509::parse_signer_dn`] — T17.Γ.1.
///
/// Matching rules (first applicable wins):
/// 1. If `alias` is 64 hex characters, it's treated as a SHA-256 digest
///    and must match a digest entry verbatim (case-insensitive).
/// 2. Otherwise (typical `"CN=Acme Corp"` or `"myAlias"`):
///    - Substring match against any DN entry (JDK loose signedBy
///      semantics — `signedBy "Acme"` matches `"CN=Acme Corp"`);
///    - Exact case-insensitive match against any legacy alias token so
///      Delta's existing short-alias policies keep working.
fn signed_by_matches<S: AsRef<str>>(grant_signed_by: Option<&str>, cert_digests: &[S]) -> bool {
    let Some(alias) = grant_signed_by else {
        return true; // no signedBy filter → applies to everyone
    };
    let alias = alias.trim();
    if alias.is_empty() {
        return true;
    }

    if is_sha256_hex(alias) {
        return cert_digests.iter().any(|d| {
            let d = d.as_ref();
            is_sha256_hex(d) && d.eq_ignore_ascii_case(alias)
        });
    }

    // DN substring match — case-sensitive because RFC 4514 DNs are
    // case-sensitive in general, but the tokens we compare here are all
    // the canonical output of our own parser (with known casing).
    for token in cert_digests {
        let token = token.as_ref();
        // An entry that looks like a DN contains '=' at least once —
        // cheap filter to skip SHA-256 digests in the mixed slice.
        if token.contains('=') && token.contains(alias) {
            return true;
        }
    }

    // Legacy fallback: exact case-insensitive alias match (preserves
    // older tests using short hex prefixes as the `signedBy` value).
    cert_digests
        .iter()
        .any(|d| d.as_ref().eq_ignore_ascii_case(alias))
}

/// True when `s` is exactly 64 lowercase/uppercase hex characters —
/// i.e. a SHA-256 fingerprint.
fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Length in bytes of the UTF-8 character whose lead byte is `b`.
/// Used by the `${...}` substituter to walk the input string in
/// character-aligned chunks without splitting a multibyte character.
fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xC0 {
        1
    }
    // continuation byte (caller guarantees aligned)
    else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
}

/// WP6.8 — resolve a `${name}` token from a policy string.  Tries
/// (in order):
///
/// 1. `${/}` is a documented JDK shortcut that expands to the
///    platform path separator (`File.separator`) — `/` on Unix,
///    `\` on Windows.
/// 2. A test-injected override registered through
///    [`set_test_property_resolver`] (test-only path; see the unit
///    tests below).
/// 3. The process environment via `cratonvm_types::flags::runtime_var`.  This catches
///    `${JAVA_HOME}`, `${HOME}`, `${USER}`, etc.  `cratonvm_types::flags::runtime_var`
///    returns `Err(NotPresent)` for unset vars, which we propagate
///    as `None` so the enclosing grant gets disabled.
///
/// We deliberately do NOT look up system properties from the running
/// VM here — at policy-load time the VM may not have parsed `-D`
/// flags yet, and embedding a back-reference to the VM's property
/// store would create a circular dependency on classes the policy
/// itself controls.  Real JDK behaviour matches: `PolicyParser`'s
/// `${...}` expansion uses `java.lang.System.getProperty` only,
/// which is sourced from the same `-Dfoo=bar` flags that the OS
/// passes via the process environment in the typical case.
fn resolve_property(name: &str) -> Option<String> {
    if name == "/" {
        return Some(std::path::MAIN_SEPARATOR.to_string());
    }
    if let Some(value) = test_property_resolver(name) {
        return Some(value);
    }
    if let Ok(value) = cratonvm_types::flags::runtime_var(name) {
        return Some(value);
    }
    None
}

/// Test-only property resolver: lets unit tests inject `${...}`
/// values without touching the process environment (which would
/// pollute parallel tests).
#[cfg(test)]
thread_local! {
    static TEST_PROPS: std::cell::RefCell<std::collections::HashMap<String, String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

#[cfg(test)]
fn test_property_resolver(name: &str) -> Option<String> {
    TEST_PROPS.with(|p| p.borrow().get(name).cloned())
}

#[cfg(not(test))]
fn test_property_resolver(_name: &str) -> Option<String> {
    None
}

/// Test-only: install / clear an override for `${name}`.  Used by the
/// unit tests below to verify substitution + disable-on-miss without
/// requiring a particular environment variable to be set.
#[cfg(test)]
pub(crate) fn set_test_property(name: &str, value: Option<&str>) {
    TEST_PROPS.with(|p| {
        let mut m = p.borrow_mut();
        match value {
            Some(v) => {
                m.insert(name.to_string(), v.to_string());
            }
            None => {
                m.remove(name);
            }
        }
    });
}

/// WP6.8 — match a grant's `principal` clauses against the calling
/// Subject's principal set.  The JDK's `PolicyFile.principalsMatch`
/// uses these rules:
///
/// 1. If the grant has no `principal` clauses, it applies to any
///    Subject (and to unauthenticated callers).
/// 2. If the grant has one or more `principal` clauses, EVERY clause
///    must match SOME entry in the Subject's principal set.  An
///    unauthenticated caller (empty subject) therefore fails any
///    grant that has at least one `principal` clause.
/// 3. Class match: exact dotted-name match, OR `*` wildcard, OR
///    slash-form converted to dot-form.
/// 4. Name match: exact string match, OR `*` wildcard.
fn principals_match(
    grant_principals: &[PrincipalEntry],
    subject_principals: &[(String, String)],
) -> bool {
    if grant_principals.is_empty() {
        return true;
    }
    for required in grant_principals {
        let required_class = normalize_class(&required.class_name);
        let required_class_ref: &str = required_class.as_ref();
        let required_name = required.name.trim();
        let mut matched = false;
        for (subj_class, subj_name) in subject_principals {
            let subj_class_norm = normalize_class(subj_class);
            let class_ok =
                required_class_ref == "*" || required_class_ref == subj_class_norm.as_ref();
            let name_ok = required_name == "*" || required_name == subj_name;
            if class_ok && name_ok {
                matched = true;
                break;
            }
        }
        if !matched {
            return false;
        }
    }
    true
}

fn actions_match(grant_actions: Option<&str>, request_actions: &str) -> bool {
    let grant_list: Vec<&str> = match grant_actions {
        None | Some("") => return request_actions.trim().is_empty(),
        Some(s) => s
            .split(',')
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .collect(),
    };
    if request_actions.trim().is_empty() {
        // An empty request is implied by any non-empty grant.
        return true;
    }
    // Every requested action must be present in the grant list.
    for req in request_actions
        .split(',')
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
    {
        if !grant_list.iter().any(|g| g.eq_ignore_ascii_case(req)) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    line: usize,
    /// WP6.8 — collected during `parse_string` whenever a `${prop}`
    /// reference fails to resolve.  The parser does NOT abort: instead
    /// the enclosing grant gets `disabled = true` and the rest of the
    /// file keeps parsing so we can report all errors at once.  This
    /// matches JDK PolicyFile behaviour and avoids the "one bad grant
    /// breaks the whole file" failure mode.
    last_string_substitution_failed: bool,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            line: 1,
            last_string_substitution_failed: false,
        }
    }

    fn parse_policy(mut self) -> Result<Policy, PolicyError> {
        let mut grants = Vec::new();
        self.skip_trivia();
        while !self.eof() {
            // keystore/keystorePasswordURL directives are tolerated by
            // skipping until the next semicolon.
            if self.consume_keyword("keystore") || self.consume_keyword("keystorePasswordURL") {
                self.skip_until(';')?;
                self.expect_char(';')?;
                self.skip_trivia();
                continue;
            }
            if !self.consume_keyword("grant") {
                return Err(self.err("expected 'grant' or 'keystore' directive"));
            }
            let grant = self.parse_grant()?;
            grants.push(grant);
            self.skip_trivia();
        }
        Ok(Policy { grants })
    }

    fn parse_grant(&mut self) -> Result<Grant, PolicyError> {
        let mut grant = Grant::default();
        // WP6.8 — track substitution failures for THIS grant only,
        // so disabling propagates per-grant rather than poisoning
        // the rest of the file.
        let saved_subst_flag = self.last_string_substitution_failed;
        self.last_string_substitution_failed = false;
        self.skip_trivia();
        // Optional header entries: codeBase "...", signedBy "...", principal ...
        loop {
            self.skip_trivia();
            if self.peek_char() == Some('{') {
                break;
            }
            if self.consume_keyword("codeBase") {
                self.skip_trivia();
                grant.code_base = Some(self.parse_string()?);
            } else if self.consume_keyword("signedBy") {
                self.skip_trivia();
                grant.signed_by = Some(self.parse_string()?);
            } else if self.consume_keyword("principal") {
                // WP6.8 — actually capture the principal clause instead
                // of dropping it on the floor.  Format:
                //   principal <ClassName> "<name>"
                //   principal "<name>"          (class defaults to *)
                //   principal *                 (both wildcard)
                let principal = self.parse_principal()?;
                grant.principals.push(principal);
            } else {
                return Err(self.err("expected 'codeBase', 'signedBy', 'principal', or '{'"));
            }
            self.skip_trivia();
            if self.peek_char() == Some(',') {
                self.bump_char();
            }
        }
        self.expect_char('{')?;
        loop {
            self.skip_trivia();
            if self.peek_char() == Some('}') {
                self.bump_char();
                break;
            }
            if !self.consume_keyword("permission") {
                return Err(self.err("expected 'permission' or '}'"));
            }
            let perm = self.parse_permission()?;
            grant.permissions.push(perm);
        }
        self.skip_trivia();
        self.expect_char(';')?;
        // WP6.8 — surface substitution-failure as a disabled grant.
        // We do NOT throw — the JDK treats `${unset.prop}` as a
        // soft failure and just skips the offending grant, preserving
        // the rest of the policy file's behaviour.
        if self.last_string_substitution_failed {
            grant.disabled = true;
        }
        self.last_string_substitution_failed = saved_subst_flag;
        Ok(grant)
    }

    /// Parse a `principal` clause.  Allowed shapes (per JDK
    /// `sun.security.provider.PolicyParser.parsePrincipal`):
    ///
    /// * `principal <Class> "<name>"`   — fully qualified
    /// * `principal * "<name>"`         — class wildcard
    /// * `principal <Class> *`          — name wildcard
    /// * `principal * *`                — both wildcards
    /// * `principal "<name>"`           — class defaults to wildcard
    fn parse_principal(&mut self) -> Result<PrincipalEntry, PolicyError> {
        self.skip_trivia();
        let mut entry = PrincipalEntry::default();

        // First token: either an identifier (class name), `*`, or a
        // bare quoted name (in which case class defaults to `*`).
        match self.peek_char() {
            Some('"') => {
                entry.class_name = "*".to_string();
                entry.name = self.parse_string()?;
                return Ok(entry);
            }
            Some('*') => {
                self.bump_char();
                entry.class_name = "*".to_string();
            }
            Some(_) => {
                entry.class_name = self.parse_identifier()?;
            }
            None => {
                return Err(self.err("expected principal class or '\"' or '*'"));
            }
        }

        self.skip_trivia();
        // Second token: name (quoted) or `*` (wildcard).
        match self.peek_char() {
            Some('"') => entry.name = self.parse_string()?,
            Some('*') => {
                self.bump_char();
                entry.name = "*".to_string();
            }
            _ => return Err(self.err("expected principal name or '*'")),
        }
        Ok(entry)
    }

    fn parse_permission(&mut self) -> Result<PermissionEntry, PolicyError> {
        let mut entry = PermissionEntry::default();
        self.skip_trivia();
        entry.class_name = self.parse_identifier()?;
        self.skip_trivia();
        // Optional target string.
        if self.peek_char() == Some('"') {
            entry.target = Some(self.parse_string()?);
            self.skip_trivia();
            if self.peek_char() == Some(',') {
                self.bump_char();
                self.skip_trivia();
                entry.actions = Some(self.parse_string()?);
                self.skip_trivia();
            }
        }
        // Tolerate `signedBy "..."` on a permission entry.
        if self.consume_keyword("signedBy") {
            self.skip_trivia();
            let _ = self.parse_string()?;
            self.skip_trivia();
        }
        self.expect_char(';')?;
        Ok(entry)
    }

    // --- Lexer primitives ---

    fn eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn peek_char(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn bump_char(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += c.len_utf8();
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn skip_trivia(&mut self) {
        loop {
            // whitespace
            while let Some(c) = self.peek_char() {
                if c.is_whitespace() {
                    self.bump_char();
                } else {
                    break;
                }
            }
            // line comment
            if self.src[self.pos..].starts_with("//") {
                while let Some(c) = self.peek_char() {
                    if c == '\n' {
                        self.bump_char();
                        break;
                    }
                    self.bump_char();
                }
                continue;
            }
            // block comment
            if self.src[self.pos..].starts_with("/*") {
                self.pos += 2;
                while !self.eof() {
                    if self.src[self.pos..].starts_with("*/") {
                        self.pos += 2;
                        break;
                    }
                    self.bump_char();
                }
                continue;
            }
            break;
        }
    }

    fn consume_keyword(&mut self, kw: &str) -> bool {
        let saved_pos = self.pos;
        let saved_line = self.line;
        self.skip_trivia();
        let remainder = &self.src[self.pos..];
        if !remainder.starts_with(kw) {
            self.pos = saved_pos;
            self.line = saved_line;
            return false;
        }
        let after = &remainder[kw.len()..];
        let is_end = match after.chars().next() {
            None => true,
            Some(c) => !c.is_alphanumeric() && c != '_',
        };
        if !is_end {
            self.pos = saved_pos;
            self.line = saved_line;
            return false;
        }
        self.pos += kw.len();
        true
    }

    fn expect_char(&mut self, expected: char) -> Result<(), PolicyError> {
        self.skip_trivia();
        match self.peek_char() {
            Some(c) if c == expected => {
                self.bump_char();
                Ok(())
            }
            Some(c) => Err(self.err(&format!("expected '{expected}', found '{c}'"))),
            None => Err(self.err(&format!("expected '{expected}', found EOF"))),
        }
    }

    fn parse_identifier(&mut self) -> Result<String, PolicyError> {
        self.skip_trivia();
        let start = self.pos;
        while let Some(c) = self.peek_char() {
            if c.is_alphanumeric() || c == '.' || c == '_' || c == '$' || c == '/' {
                self.bump_char();
            } else {
                break;
            }
        }
        if start == self.pos {
            return Err(self.err("expected identifier"));
        }
        Ok(self.src[start..self.pos].to_string())
    }

    fn parse_string(&mut self) -> Result<String, PolicyError> {
        self.skip_trivia();
        if self.peek_char() != Some('"') {
            return Err(self.err("expected '\"'"));
        }
        self.bump_char();
        let mut out = String::new();
        while let Some(c) = self.peek_char() {
            match c {
                '"' => {
                    self.bump_char();
                    // WP6.8 — `${prop}` substitution happens AFTER the
                    // string is fully tokenized so escape sequences
                    // can produce literal `${` if the policy author
                    // wants one.  The JDK does the substitution at
                    // load-time; doing it post-parse means any
                    // `\${` escape works as expected, and a single
                    // unset reference disables the whole grant via
                    // the parser-level flag.
                    return Ok(self.substitute_properties(&out));
                }
                '\\' => {
                    self.bump_char();
                    match self.peek_char() {
                        Some('n') => out.push('\n'),
                        Some('r') => out.push('\r'),
                        Some('t') => out.push('\t'),
                        Some('"') => out.push('"'),
                        Some('\\') => out.push('\\'),
                        // WP6.8: `\$` lets the policy author embed a
                        // literal `$` without triggering ${...}
                        // substitution.  Useful for legacy paths that
                        // contain a stray dollar sign.
                        // Preserve the escape until substitution has scanned
                        // the completed string. Otherwise `\${USER}` becomes
                        // `${USER}` here and is mistakenly expanded below.
                        Some('$') => {
                            out.push('\\');
                            out.push('$');
                        }
                        Some(other) => out.push(other),
                        None => return Err(self.err("unterminated escape")),
                    }
                    self.bump_char();
                }
                _ => {
                    out.push(c);
                    self.bump_char();
                }
            }
        }
        Err(self.err("unterminated string literal"))
    }

    /// WP6.8 — expand `${name}` references using
    /// `cratonvm_types::flags::runtime_var(name)` (so JDK's `-Djava.home=...` and any
    /// shell-set environment variable both work as substitution
    /// sources).  The resolution order matches JDK
    /// `PolicyParser.expand`:
    ///
    /// 1. If `name` resolves via `cratonvm_types::flags::runtime_var`, use that value.
    /// 2. If `name == "/"` it expands to the platform path
    ///    separator (`File.separator` in Java).
    /// 3. Otherwise the substitution fails — the caller surfaces
    ///    this as `disabled = true` on the enclosing grant.
    ///
    /// The returned string keeps any literal characters that were
    /// not part of a `${...}` token, so a value like
    /// `"file:${java.home}/lib/-"` becomes
    /// `"file:/opt/jdk/lib/-"` when `java.home=/opt/jdk`.
    fn substitute_properties(&mut self, raw: &str) -> String {
        // Fast path: a string with no `$` cannot contain a
        // substitution, so skip the scan entirely.  This is the
        // common case for permission targets like `"/tmp/*"`.
        if !raw.contains('$') {
            return raw.to_string();
        }
        let mut out = String::with_capacity(raw.len());
        let bytes = raw.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'$' {
                // `parse_string` preserves `\$` solely as a substitution
                // escape marker. Consume it here and publish the literal.
                out.push('$');
                i += 2;
            } else if bytes[i] == b'\\' {
                // A backslash that did not escape a dollar is ordinary
                // policy text. Handle it separately so the copy run below
                // cannot consume the escape marker before this branch sees
                // it on the next iteration.
                out.push('\\');
                i += 1;
            } else if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                // Find the matching `}` — search byte-wise because
                // `}` is ASCII and cannot appear inside a UTF-8
                // continuation byte (those have the high bit set).
                let start = i + 2;
                let mut end = start;
                while end < bytes.len() && bytes[end] != b'}' {
                    end += 1;
                }
                if end >= bytes.len() {
                    // Unmatched `${` — preserve verbatim so the
                    // policy author can see the typo, but do NOT
                    // mark the grant as disabled (this is a
                    // syntax confusion, not a substitution failure).
                    out.push_str(&raw[i..]);
                    return out;
                }
                let name = &raw[start..end];
                match resolve_property(name) {
                    Some(value) => out.push_str(&value),
                    None => {
                        // The grant becomes inactive.  We still
                        // preserve the literal token so debug logs
                        // can show what was attempted.
                        self.last_string_substitution_failed = true;
                        out.push_str("${");
                        out.push_str(name);
                        out.push('}');
                    }
                }
                i = end + 1;
            } else {
                // Walk forward to the next non-ASCII / non-`$`
                // boundary in one shot, copying the run as a UTF-8
                // slice so we never split a multibyte character.
                // We stop at any `$` so the next iteration can
                // examine it for `${` substitution.
                let run_start = i;
                while i < bytes.len() && bytes[i] != b'$' && bytes[i] != b'\\' {
                    // Step by the UTF-8 char width to keep ASCII /
                    // non-ASCII distinction intact.  `from_utf8`
                    // verified the slice on entry so this is safe.
                    let ch_len = utf8_char_len(bytes[i]);
                    i += ch_len;
                }
                out.push_str(&raw[run_start..i]);
            }
        }
        out
    }

    fn skip_until(&mut self, target: char) -> Result<(), PolicyError> {
        while let Some(c) = self.peek_char() {
            if c == target {
                return Ok(());
            }
            self.bump_char();
        }
        Err(self.err(&format!("expected '{target}' before EOF")))
    }

    fn skip_until_any(&mut self, targets: &[char]) -> Result<(), PolicyError> {
        while let Some(c) = self.peek_char() {
            if targets.contains(&c) {
                return Ok(());
            }
            self.bump_char();
        }
        Err(self.err("unexpected EOF"))
    }

    fn err(&self, message: &str) -> PolicyError {
        PolicyError::Parse {
            line: self.line,
            message: message.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn test_parse_empty() {
        let p = Policy::parse("").unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn test_parse_single_grant() {
        let src = r#"
            grant codeBase "file:/home/app/-" {
                permission java.io.FilePermission "/tmp/*", "read,write";
                permission java.net.SocketPermission "*:80", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert_eq!(p.grants.len(), 1);
        let g = &p.grants[0];
        assert_eq!(g.code_base.as_deref(), Some("file:/home/app/-"));
        assert_eq!(g.permissions.len(), 2);
        let p0 = &g.permissions[0];
        assert_eq!(p0.class_name, "java.io.FilePermission");
        assert_eq!(p0.target.as_deref(), Some("/tmp/*"));
        assert_eq!(p0.actions.as_deref(), Some("read,write"));
        let p1 = &g.permissions[1];
        assert_eq!(p1.class_name, "java.net.SocketPermission");
        assert_eq!(p1.target.as_deref(), Some("*:80"));
        assert_eq!(p1.actions.as_deref(), Some("connect"));
    }

    #[test]
    fn test_parse_multiple_grants() {
        let src = r#"
            grant {
                permission java.security.AllPermission;
            };
            grant codeBase "file:/plugins/-" {
                permission java.io.FilePermission "/var/log/-", "read";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert_eq!(p.grants.len(), 2);
        assert!(p.grants[0].code_base.is_none());
        assert_eq!(
            p.grants[0].permissions[0].class_name,
            "java.security.AllPermission"
        );
        assert_eq!(p.grants[1].code_base.as_deref(), Some("file:/plugins/-"));
    }

    #[test]
    fn test_parse_with_comments() {
        let src = r#"
            // top-level comment
            grant {
                /* block
                   comment */
                permission java.lang.RuntimePermission "createClassLoader";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert_eq!(p.grants.len(), 1);
        assert_eq!(
            p.grants[0].permissions[0].target.as_deref(),
            Some("createClassLoader")
        );
    }

    #[test]
    fn test_parse_permission_without_target() {
        let src = r#"
            grant {
                permission java.security.AllPermission;
            };
        "#;
        let p = Policy::parse(src).unwrap();
        let perm = &p.grants[0].permissions[0];
        assert_eq!(perm.class_name, "java.security.AllPermission");
        assert!(perm.target.is_none());
        assert!(perm.actions.is_none());
    }

    #[test]
    fn test_parse_escape_sequences() {
        let src = r#"
            grant {
                permission java.io.FilePermission "C:\\Users\\x", "read";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert_eq!(
            p.grants[0].permissions[0].target.as_deref(),
            Some(r"C:\Users\x")
        );
    }

    #[test]
    fn test_parse_error_missing_brace() {
        let src = r#"
            grant {
                permission java.security.AllPermission
            };
        "#;
        let err = Policy::parse(src).unwrap_err();
        assert!(matches!(err, PolicyError::Parse { .. }));
    }

    #[test]
    fn test_parse_error_bad_directive() {
        let err = Policy::parse("wibble {};").unwrap_err();
        match err {
            PolicyError::Parse { message, .. } => assert!(message.contains("grant")),
            _ => panic!("expected Parse error"),
        }
    }

    #[test]
    fn test_parse_signed_by() {
        let src = r#"
            grant signedBy "myAlias" {
                permission java.security.AllPermission;
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert_eq!(p.grants[0].signed_by.as_deref(), Some("myAlias"));
    }

    // -- implies() tests --

    #[test]
    fn test_implies_all_permission() {
        let src = r#"grant { permission java.security.AllPermission; };"#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies("java.io.FilePermission", "/etc/passwd", "read", None));
        assert!(p.implies("java.net.SocketPermission", "*:80", "connect", None));
    }

    #[test]
    fn test_implies_specific_permission() {
        let src = r#"
            grant {
                permission java.io.FilePermission "/tmp/*", "read,write";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies("java.io.FilePermission", "/tmp/x", "read", None));
        assert!(p.implies("java.io.FilePermission", "/tmp/y", "write", None));
        assert!(!p.implies("java.io.FilePermission", "/etc/x", "read", None));
        assert!(!p.implies("java.io.FilePermission", "/tmp/x", "execute", None));
    }

    #[test]
    fn test_implies_recursive_wildcard() {
        let src = r#"
            grant {
                permission java.io.FilePermission "/var/log/-", "read";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies(
            "java.io.FilePermission",
            "/var/log/app/info.txt",
            "read",
            None
        ));
        assert!(p.implies("java.io.FilePermission", "/var/log/x", "read", None));
        assert!(!p.implies("java.io.FilePermission", "/etc/x", "read", None));
    }

    #[test]
    fn test_implies_code_base_filter() {
        let src = r#"
            grant codeBase "file:/home/app/-" {
                permission java.io.FilePermission "/tmp/*", "read";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        // Matches when codeBase is within /home/app.
        assert!(p.implies(
            "java.io.FilePermission",
            "/tmp/x",
            "read",
            Some("file:/home/app/lib/x.jar")
        ));
        // Does not match when codeBase is elsewhere.
        assert!(!p.implies(
            "java.io.FilePermission",
            "/tmp/x",
            "read",
            Some("file:/opt/other/y.jar")
        ));
        // Does not match when no codeBase is in scope.
        assert!(!p.implies("java.io.FilePermission", "/tmp/x", "read", None));
    }

    #[test]
    fn test_implies_socket_wildcard_port() {
        let src = r#"
            grant {
                permission java.net.SocketPermission "*:80", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies(
            "java.net.SocketPermission",
            "example.com:80",
            "connect",
            None
        ));
        assert!(!p.implies(
            "java.net.SocketPermission",
            "example.com:443",
            "connect",
            None
        ));
    }

    #[test]
    fn test_implies_actions_case_insensitive_subset() {
        let src = r#"
            grant {
                permission java.io.FilePermission "/tmp/*", "read,write,execute";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies("java.io.FilePermission", "/tmp/a", "READ", None));
        assert!(p.implies("java.io.FilePermission", "/tmp/a", "read,write", None));
        assert!(!p.implies("java.io.FilePermission", "/tmp/a", "delete", None));
    }

    #[test]
    fn test_implies_slash_class_form() {
        let src = r#"grant { permission java.io.FilePermission "/x", "read"; };"#;
        let p = Policy::parse(src).unwrap();
        // Both forms resolve to the same permission.
        assert!(p.implies("java/io/FilePermission", "/x", "read", None));
        assert!(p.implies("java.io.FilePermission", "/x", "read", None));
    }

    #[test]
    fn test_from_file_roundtrip() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("cratonvm-policy-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.policy");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(
                f,
                "grant codeBase \"file:/home/app/-\" {{\n    permission java.io.FilePermission \"/tmp/*\", \"read,write\";\n    permission java.net.SocketPermission \"*:80\", \"connect\";\n}};"
            )
            .unwrap();
        }
        let p = Policy::from_file(&path).unwrap();
        assert_eq!(p.grants.len(), 1);
        assert_eq!(p.grants[0].permissions.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_code_base_single_wildcard() {
        // `/*` (single segment) vs `/-` (recursive).
        let src = r#"
            grant codeBase "file:/apps/*" {
                permission java.security.AllPermission;
            };
        "#;
        let p = Policy::parse(src).unwrap();
        // Direct child only.
        assert!(p.implies(
            "java.io.FilePermission",
            "/x",
            "read",
            Some("file:/apps/foo")
        ));
        // Not recursive.
        assert!(!p.implies(
            "java.io.FilePermission",
            "/x",
            "read",
            Some("file:/apps/foo/bar")
        ));
    }

    #[test]
    fn test_empty_actions() {
        let src = r#"
            grant {
                permission java.lang.RuntimePermission "getProtectionDomain";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies(
            "java.lang.RuntimePermission",
            "getProtectionDomain",
            "",
            None
        ));
    }

    #[test]
    fn test_keystore_directive_ignored() {
        let src = r#"
            keystore "file:keystore.jks", "jks";
            grant {
                permission java.security.AllPermission;
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert_eq!(p.grants.len(), 1);
    }

    // -----------------------------------------------------------------------
    // T11 · Extended SocketPermission target grammar.
    // -----------------------------------------------------------------------

    #[test]
    fn t11_socket_permission_exact_host_port() {
        let src = r#"
            grant {
                permission java.net.SocketPermission "api.example.com:443", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies(
            "java.net.SocketPermission",
            "api.example.com:443",
            "connect",
            None
        ));
        assert!(!p.implies(
            "java.net.SocketPermission",
            "api.example.com:80",
            "connect",
            None
        ));
        assert!(!p.implies(
            "java.net.SocketPermission",
            "other.example.com:443",
            "connect",
            None
        ));
    }

    #[test]
    fn t11_socket_permission_port_range() {
        let src = r#"
            grant {
                permission java.net.SocketPermission "*:8080-8090", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        for port in 8080..=8090 {
            assert!(
                p.implies(
                    "java.net.SocketPermission",
                    &format!("host.example:{port}"),
                    "connect",
                    None,
                ),
                "port {port} should be allowed"
            );
        }
        assert!(!p.implies(
            "java.net.SocketPermission",
            "host.example:8079",
            "connect",
            None
        ));
        assert!(!p.implies(
            "java.net.SocketPermission",
            "host.example:8091",
            "connect",
            None
        ));
    }

    #[test]
    fn t11_socket_permission_domain_wildcard() {
        let src = r#"
            grant {
                permission java.net.SocketPermission "*.example.com:443", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies(
            "java.net.SocketPermission",
            "api.example.com:443",
            "connect",
            None
        ));
        assert!(p.implies(
            "java.net.SocketPermission",
            "deep.nested.example.com:443",
            "connect",
            None
        ));
        assert!(p.implies(
            "java.net.SocketPermission",
            "example.com:443",
            "connect",
            None
        ));
        assert!(!p.implies(
            "java.net.SocketPermission",
            "otherexample.com:443",
            "connect",
            None
        ));
    }

    #[test]
    fn t11_socket_permission_cidr_matches() {
        // /8 CIDR — entire first-octet 10.* block.
        let src = r#"
            grant {
                permission java.net.SocketPermission "10.0.0.0/8:80", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies("java.net.SocketPermission", "10.1.2.3:80", "connect", None));
        assert!(p.implies(
            "java.net.SocketPermission",
            "10.255.255.255:80",
            "connect",
            None
        ));
        assert!(!p.implies("java.net.SocketPermission", "11.0.0.1:80", "connect", None));
        // Port mismatch still denies.
        assert!(!p.implies("java.net.SocketPermission", "10.1.2.3:443", "connect", None));
    }

    #[test]
    fn t11_socket_permission_cidr_private_16() {
        let src = r#"
            grant {
                permission java.net.SocketPermission "192.168.0.0/16", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        // /16 + no port → any port OK.
        assert!(p.implies(
            "java.net.SocketPermission",
            "192.168.1.10:22",
            "connect",
            None
        ));
        assert!(p.implies(
            "java.net.SocketPermission",
            "192.168.255.1:8443",
            "connect",
            None
        ));
        assert!(!p.implies("java.net.SocketPermission", "10.0.0.1:22", "connect", None));
    }

    #[test]
    fn t11_socket_permission_localhost_alias() {
        let src = r#"
            grant {
                permission java.net.SocketPermission "localhost:8080", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies(
            "java.net.SocketPermission",
            "localhost:8080",
            "connect",
            None
        ));
        assert!(p.implies(
            "java.net.SocketPermission",
            "127.0.0.1:8080",
            "connect",
            None
        ));
        assert!(p.implies("java.net.SocketPermission", "::1:8080", "connect", None));
        assert!(!p.implies(
            "java.net.SocketPermission",
            "example.com:8080",
            "connect",
            None
        ));
    }

    #[test]
    fn t11_socket_permission_still_supports_wildcard_port() {
        // Regression: `"*:<port>"` wildcard must keep working.
        let src = r#"
            grant {
                permission java.net.SocketPermission "*:80", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies(
            "java.net.SocketPermission",
            "anything.example.com:80",
            "connect",
            None
        ));
    }

    #[test]
    fn t11_signed_by_blocks_unmatched_signer() {
        // When a grant has a signedBy clause, classes without that signer
        // digest must NOT pick up its permissions.
        let src = r#"
            grant signedBy "abc123" {
                permission java.io.FilePermission "/tmp/*", "read";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        // Unsigned class: no digests → grant filtered out.
        assert!(!p.implies_full(
            "java.io.FilePermission",
            "/tmp/x",
            "read",
            None,
            &[] as &[&str],
        ));
        // Wrong signer digest → filtered out.
        assert!(!p.implies_full(
            "java.io.FilePermission",
            "/tmp/x",
            "read",
            None,
            &["deadbeef".to_string()],
        ));
        // Correct signer digest → grant applies.
        assert!(p.implies_full(
            "java.io.FilePermission",
            "/tmp/x",
            "read",
            None,
            &["abc123".to_string()],
        ));
    }

    #[test]
    fn t11_signed_by_case_insensitive() {
        let src = r#"
            grant signedBy "ABCDEF" {
                permission java.security.AllPermission;
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies_full(
            "java.io.FilePermission",
            "/x",
            "read",
            None,
            &["abcdef".to_string()],
        ));
    }

    #[test]
    fn t11_port_range_open_ended() {
        // JDK accepts `-8080` (all ports up to 8080) and `8080-` (8080+).
        let src = r#"
            grant {
                permission java.net.SocketPermission "host:-1024", "connect";
                permission java.net.SocketPermission "host:1025-", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies("java.net.SocketPermission", "host:80", "connect", None));
        assert!(p.implies("java.net.SocketPermission", "host:1024", "connect", None));
        assert!(p.implies("java.net.SocketPermission", "host:65535", "connect", None));
    }

    // -----------------------------------------------------------------------
    // T17.Γ · X.509 Subject DN + IPv6 CIDR.
    // -----------------------------------------------------------------------

    /// Build a PKCS#7 signer blob whose signer cert has the given CN.
    fn make_pkcs7_with_cn(cn: &str) -> Vec<u8> {
        use super::super::x509::builder::*;
        // 2.5.4.3 is id-at-commonName.
        let cn_oid = [0x55, 0x04, 0x03];
        let subject = name(&[rdn(&atv(&cn_oid, &printable_string(cn)))]);
        let cert = certificate(&subject);
        pkcs7_signed_data(&cert)
    }

    fn dn_of(pkcs7: &[u8]) -> String {
        x509::parse_signer_dn(pkcs7).expect("pkcs7 must parse")
    }

    #[test]
    fn t17_g_signed_by_dn_matches_cert_subject() {
        // grant signedBy "CN=Acme Corp" must match a cert whose subject
        // renders as exactly "CN=Acme Corp".
        let src = r#"
            grant signedBy "CN=Acme Corp" {
                permission java.io.FilePermission "/opt/*", "read";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        let pkcs7 = make_pkcs7_with_cn("Acme Corp");
        let dn = dn_of(&pkcs7);
        assert_eq!(dn, "CN=Acme Corp");
        assert!(p.implies_full("java.io.FilePermission", "/opt/a", "read", None, &[dn],));
    }

    #[test]
    fn t17_g_signed_by_dn_substring_match() {
        // JDK's signedBy is a loose substring match.
        let src = r#"
            grant signedBy "Acme" {
                permission java.io.FilePermission "/opt/*", "read";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        let pkcs7 = make_pkcs7_with_cn("Acme Corp");
        let dn = dn_of(&pkcs7);
        assert!(p.implies_full("java.io.FilePermission", "/opt/a", "read", None, &[dn],));
    }

    #[test]
    fn t17_g_signed_by_no_match_denied() {
        // signedBy "CN=Evil" against a cert whose DN is "CN=Acme" →
        // grant does not apply.
        let src = r#"
            grant signedBy "CN=Evil" {
                permission java.io.FilePermission "/opt/*", "read";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        let pkcs7 = make_pkcs7_with_cn("Acme");
        let dn = dn_of(&pkcs7);
        assert!(!p.implies_full("java.io.FilePermission", "/opt/a", "read", None, &[dn],));
    }

    #[test]
    fn t17_g_signed_by_malformed_pkcs7_returns_err() {
        // Random bytes must not panic — the parser rejects them.
        let garbage: Vec<u8> = (0..256).map(|i| (i * 7 + 13) as u8).collect();
        let err = x509::parse_signer_dn(&garbage).unwrap_err();
        assert!(matches!(
            err,
            x509::X509Error::NotPkcs7
                | x509::X509Error::MalformedAsn1
                | x509::X509Error::NoSignerCert
        ));
    }

    #[test]
    fn t17_g_ipv6_cidr_matches_loopback() {
        let src = r#"
            grant {
                permission java.net.SocketPermission "::1/128", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies("java.net.SocketPermission", "[::1]:443", "connect", None));
        // Non-loopback must not match a /128.
        assert!(!p.implies(
            "java.net.SocketPermission",
            "[2001:db8::1]:443",
            "connect",
            None
        ));
    }

    #[test]
    fn t17_g_ipv6_cidr_fe80_matches_link_local() {
        let src = r#"
            grant {
                permission java.net.SocketPermission "fe80::/10", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies("java.net.SocketPermission", "[fe80::1]:80", "connect", None));
        // Another link-local address in the block.
        assert!(p.implies(
            "java.net.SocketPermission",
            "[febf:ffff::]:80",
            "connect",
            None
        ));
        // 2001:db8::/32 is global scope, must not match.
        assert!(!p.implies(
            "java.net.SocketPermission",
            "[2001:db8::1]:80",
            "connect",
            None
        ));
    }

    #[test]
    fn t17_g_ipv6_port_range_bracketed() {
        let src = r#"
            grant {
                permission java.net.SocketPermission "[::1]:8080-8090", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies("java.net.SocketPermission", "[::1]:8085", "connect", None));
        assert!(!p.implies("java.net.SocketPermission", "[::1]:9000", "connect", None));
    }

    #[test]
    fn t17_g_ipv4_still_works() {
        // Regression: Delta's IPv4 CIDR path must stay intact.
        let src = r#"
            grant {
                permission java.net.SocketPermission "10.0.0.0/8", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies("java.net.SocketPermission", "10.1.2.3:80", "connect", None));
        assert!(!p.implies("java.net.SocketPermission", "11.0.0.1:80", "connect", None));
    }

    #[test]
    fn t17_g_ipv6_any_prefix_matches_everything() {
        // ::/0 is the IPv6 "match anywhere" CIDR.
        let src = r#"
            grant {
                permission java.net.SocketPermission "::/0", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.implies(
            "java.net.SocketPermission",
            "[2001:db8::cafe]:8080",
            "connect",
            None
        ));
        // v4-mapped form also ok.
        assert!(p.implies(
            "java.net.SocketPermission",
            "[::ffff:10.0.0.1]:80",
            "connect",
            None
        ));
    }

    #[test]
    fn t17_g_ipv6_malformed_forms_do_not_match() {
        let src = r#"
            grant {
                permission java.net.SocketPermission "::1/128", "connect";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        // Triple-colon and double-compression are invalid per RFC 4291;
        // std::net::Ipv6Addr rejects them, so matching returns false.
        assert!(!p.implies("java.net.SocketPermission", "[:::1]:443", "connect", None));
        assert!(!p.implies("java.net.SocketPermission", "[::1::]:443", "connect", None));
    }

    #[test]
    fn t17_g_signed_by_digest_mode_preserved() {
        // 64 hex chars → SHA-256 digest mode.  DN strings in the slice
        // must NOT match a 64-hex alias.
        let digest = "a".repeat(64);
        let src_template = format!(
            r#"grant signedBy "{digest}" {{ permission java.io.FilePermission "/x", "read"; }};"#
        );
        let p = Policy::parse(&src_template).unwrap();
        // DN present but no matching digest → denied.
        assert!(!p.implies_full(
            "java.io.FilePermission",
            "/x",
            "read",
            None,
            &["CN=Acme Corp".to_string()],
        ));
        // Correct digest → allowed.
        assert!(p.implies_full(
            "java.io.FilePermission",
            "/x",
            "read",
            None,
            &[digest.clone()],
        ));
    }

    // -----------------------------------------------------------------------
    // WP6.8 · Property substitution + principal enforcement.
    // -----------------------------------------------------------------------

    #[test]
    fn wp68_substitution_resolves_known_property() {
        set_test_property("policy.test.home", Some("/opt/myapp"));
        let src = r#"
            grant codeBase "file:${policy.test.home}/lib/-" {
                permission java.io.FilePermission "/tmp/*", "read";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        // Substituted code base must be the resolved path.
        assert_eq!(p.grants.len(), 1);
        assert!(!p.grants[0].disabled);
        assert_eq!(
            p.grants[0].code_base.as_deref(),
            Some("file:/opt/myapp/lib/-")
        );
        // And it must actually match a request from that code base.
        assert!(p.implies(
            "java.io.FilePermission",
            "/tmp/x",
            "read",
            Some("file:/opt/myapp/lib/x.jar"),
        ));
        set_test_property("policy.test.home", None);
    }

    #[test]
    fn wp68_substitution_failure_disables_grant() {
        // Make sure no test_property is set for this name.
        set_test_property("policy.unset.var.zzz", None);
        let src = r#"
            grant codeBase "file:${policy.unset.var.zzz}/-" {
                permission java.security.AllPermission;
            };
            grant {
                permission java.io.FilePermission "/safe/*", "read";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert_eq!(p.grants.len(), 2);
        assert!(
            p.grants[0].disabled,
            "grant with unset ${{...}} must be disabled"
        );
        assert!(!p.grants[1].disabled, "second grant must be unaffected");
        // The disabled grant cannot be used to satisfy a permission.
        assert!(!p.implies(
            "java.security.AllPermission",
            "",
            "",
            Some("file:/anything/-"),
        ));
        // The unaffected grant still works.
        assert!(p.implies("java.io.FilePermission", "/safe/x", "read", None));
    }

    #[test]
    fn wp68_substitution_path_separator_shortcut() {
        let src = r#"
            grant codeBase "file:${/}usr${/}share${/}-" {
                permission java.security.AllPermission;
            };
        "#;
        let p = Policy::parse(src).unwrap();
        let cb = p.grants[0].code_base.as_deref().unwrap();
        // Platform-correct separator was inserted.
        let sep = std::path::MAIN_SEPARATOR.to_string();
        let expected = format!("file:{sep}usr{sep}share{sep}-");
        assert_eq!(cb, expected);
    }

    #[test]
    fn wp68_substitution_dollar_escape_preserves_literal() {
        // `\$` must produce a literal `$` and NOT trigger
        // substitution of `${USER}`, even if that env var is set.
        let src = r#"
            grant {
                permission java.io.FilePermission "/tmp/file_\${USER}_x", "read";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert_eq!(
            p.grants[0].permissions[0].target.as_deref(),
            Some("/tmp/file_${USER}_x")
        );
    }

    #[test]
    fn wp68_substitution_unset_var_in_permission_disables_grant() {
        // Substitution failure inside a permission target also
        // disables the surrounding grant.
        set_test_property("wp68.unset.var", None);
        let src = r#"
            grant {
                permission java.io.FilePermission "${wp68.unset.var}", "read";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert_eq!(p.grants.len(), 1);
        assert!(p.grants[0].disabled);
        // Negative implication: the grant is dead, so even an exact
        // literal target must not match.
        assert!(!p.implies("java.io.FilePermission", "${wp68.unset.var}", "read", None,));
    }

    #[test]
    fn wp68_principal_clause_now_captured() {
        // Old code dropped `principal ...` on the floor; this test
        // pins the new behaviour where the clause becomes a Subject
        // filter.
        let src = r#"
            grant principal javax.security.auth.x500.X500Principal "CN=Alice" {
                permission java.io.FilePermission "/secret/*", "read";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert_eq!(p.grants.len(), 1);
        assert_eq!(p.grants[0].principals.len(), 1);
        assert_eq!(
            p.grants[0].principals[0].class_name,
            "javax.security.auth.x500.X500Principal"
        );
        assert_eq!(p.grants[0].principals[0].name, "CN=Alice");
    }

    #[test]
    fn wp68_principal_filter_blocks_unauthenticated_caller() {
        // A grant with a `principal` clause must NOT match when the
        // caller has no Subject.  This is the security-critical
        // behaviour — the previous parser silently turned this grant
        // into an unrestricted wildcard.
        let src = r#"
            grant principal javax.security.auth.x500.X500Principal "CN=Alice" {
                permission java.io.FilePermission "/secret/*", "read";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        // No subject → grant must not apply.
        assert!(!p.implies_full_with_principals(
            "java.io.FilePermission",
            "/secret/x",
            "read",
            None,
            &[] as &[&str],
            &[],
        ));
        // Wrong principal → grant must not apply.
        assert!(!p.implies_full_with_principals(
            "java.io.FilePermission",
            "/secret/x",
            "read",
            None,
            &[] as &[&str],
            &[(
                "javax.security.auth.x500.X500Principal".to_string(),
                "CN=Bob".to_string(),
            )],
        ));
        // Matching principal → grant applies.
        assert!(p.implies_full_with_principals(
            "java.io.FilePermission",
            "/secret/x",
            "read",
            None,
            &[] as &[&str],
            &[(
                "javax.security.auth.x500.X500Principal".to_string(),
                "CN=Alice".to_string(),
            )],
        ));
    }

    #[test]
    fn wp68_principal_class_and_name_wildcards() {
        let src = r#"
            grant principal * "CN=Anyone" {
                permission java.lang.RuntimePermission "doX";
            };
            grant principal javax.security.auth.x500.X500Principal * {
                permission java.lang.RuntimePermission "doY";
            };
            grant principal * * {
                permission java.lang.RuntimePermission "doZ";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert_eq!(p.grants.len(), 3);

        // First grant: any class with name "CN=Anyone".
        let alice = ("com.acme.MyPrincipal".to_string(), "CN=Anyone".to_string());
        assert!(p.implies_full_with_principals(
            "java.lang.RuntimePermission",
            "doX",
            "",
            None,
            &[] as &[&str],
            &[alice.clone()],
        ));

        // Second grant: X500Principal with any name.
        let bob = (
            "javax.security.auth.x500.X500Principal".to_string(),
            "CN=Bob".to_string(),
        );
        assert!(p.implies_full_with_principals(
            "java.lang.RuntimePermission",
            "doY",
            "",
            None,
            &[] as &[&str],
            &[bob.clone()],
        ));

        // Third grant: matches everyone (still requires a subject).
        let any = ("X".to_string(), "Y".to_string());
        assert!(p.implies_full_with_principals(
            "java.lang.RuntimePermission",
            "doZ",
            "",
            None,
            &[] as &[&str],
            &[any.clone()],
        ));
        // But still rejects unauthenticated callers (every grant
        // here has at least one principal clause).
        assert!(!p.implies_full_with_principals(
            "java.lang.RuntimePermission",
            "doZ",
            "",
            None,
            &[] as &[&str],
            &[],
        ));
    }

    #[test]
    fn wp68_multiple_principals_require_all_match() {
        // JDK semantics: multi-principal grants are conjunctive.
        let src = r#"
            grant principal A "alpha", principal B "beta" {
                permission java.lang.RuntimePermission "doIt";
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert_eq!(p.grants[0].principals.len(), 2);
        // Only one principal → not enough.
        assert!(!p.implies_full_with_principals(
            "java.lang.RuntimePermission",
            "doIt",
            "",
            None,
            &[] as &[&str],
            &[("A".to_string(), "alpha".to_string())],
        ));
        // Both principals → matches.
        assert!(p.implies_full_with_principals(
            "java.lang.RuntimePermission",
            "doIt",
            "",
            None,
            &[] as &[&str],
            &[
                ("A".to_string(), "alpha".to_string()),
                ("B".to_string(), "beta".to_string()),
            ],
        ));
    }

    #[test]
    fn wp68_disabled_grant_not_used_via_signedby_path_either() {
        // Belt-and-braces: a disabled grant must not produce a match
        // when consulted via the cert-digest path.
        set_test_property("wp68.unset.signed", None);
        let src = r#"
            grant signedBy "abc" codeBase "${wp68.unset.signed}" {
                permission java.security.AllPermission;
            };
        "#;
        let p = Policy::parse(src).unwrap();
        assert!(p.grants[0].disabled);
        assert!(!p.implies_full(
            "java.io.FilePermission",
            "/x",
            "read",
            Some("any"),
            &["abc".to_string()],
        ));
    }
}
