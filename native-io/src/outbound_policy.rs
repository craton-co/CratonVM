// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Outbound-host policy hook and connect-timeout knob (task #16).
//!
//! SSRF hardening for the blocking NIO connect path. Guest code that
//! resolves to a link-local cloud-metadata address (e.g. AWS EC2 IMDS
//! at `169.254.169.254`) would previously hang the VM thread for the
//! OS-default TCP connect timeout (~2 minutes on Linux, longer on
//! Windows). The async path already capped this at 30 seconds; this
//! module brings the blocking path to parity AND adds a policy hook
//! that lets an embedder reject suspicious destinations outright.
//!
//! Two concerns, one module:
//!
//!   1. **Connect timeout** — `connect_timeout()` returns the configured
//!      duration. Default is 30 s (matching the async path). Embedders
//!      override via `set_connect_timeout`. The hot connect call sites
//!      in `socket_channel.rs` and `net.rs` use
//!      `TcpStream::connect_timeout` with this value.
//!
//!   2. **Allow hook** — `policy_allow(target)` is consulted before
//!      every outbound connect. The default impl rejects the well-known
//!      cloud-metadata link-local addresses; `set_policy` lets an
//!      embedder install a stricter (or more permissive) rule.
//!
//! Both knobs are process-global `OnceLock`-style cells. They are
//! intentionally one-impl-fn-pointer style (no trait object) — the
//! native-io crate already pays an `OnceLock` per registry, and a
//! `fn`-pointer keeps the hot path branch-predictor-friendly.

use crate::io_flags;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Connect timeout (default 30 s; configurable via `set_connect_timeout`)
// ---------------------------------------------------------------------------

/// Default connect timeout in milliseconds. Matches the async-socket
/// pool's hard cap so the blocking path can't outlive the async one.
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 30_000;

/// Encoded as milliseconds in an atomic so reads on the hot path are
/// lock-free. A value of 0 means "use platform default" (we still
/// substitute `DEFAULT_CONNECT_TIMEOUT_MS` to guarantee a finite cap).
static CONNECT_TIMEOUT_MS: AtomicU64 = AtomicU64::new(DEFAULT_CONNECT_TIMEOUT_MS);

/// Returns the configured connect timeout. Always finite — even if an
/// embedder calls `set_connect_timeout(Duration::ZERO)` we fall back
/// to the 30 s default so the blocking thread can't hang indefinitely.
pub fn connect_timeout() -> Duration {
    let ms = CONNECT_TIMEOUT_MS.load(Ordering::Relaxed);
    if ms == 0 {
        Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS)
    } else {
        Duration::from_millis(ms)
    }
}

/// Override the connect timeout. Embedders (and tests) call this once
/// at VM startup. Durations longer than `u64::MAX` ms are clamped, and
/// the zero duration is treated as "reset to default" (see
/// `connect_timeout`).
pub fn set_connect_timeout(d: Duration) {
    let ms: u64 = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    CONNECT_TIMEOUT_MS.store(ms, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Outbound-host policy hook
// ---------------------------------------------------------------------------

/// Decision returned by an outbound-policy check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Allow the connect to proceed.
    Allow,
    /// Reject; the connect site translates this into a Java
    /// `IOException("connect denied by outbound policy: <reason>")`.
    Deny(String),
}

/// Policy function pointer. Takes the resolved-or-textual target host
/// and port string (e.g. `"169.254.169.254:80"` or `"metadata.google.internal:80"`)
/// and returns a decision. Implementations should be cheap — they run on
/// every connect.
pub type PolicyFn = fn(target: &str) -> PolicyDecision;

/// Pointer to the active policy. `AtomicU64` storing a function-pointer
/// is the closest portable thing to `Atomic<fn>` — on every supported
/// 64-bit target `fn` is a `usize` and we can round-trip via `transmute`
/// at the boundary. We keep that unsafety isolated to two tiny helpers.
static POLICY_FN: AtomicU64 = AtomicU64::new(0);

fn store_policy(f: PolicyFn) {
    // Safety: `PolicyFn` and `usize` have the same size on all supported
    // platforms; we round-trip via `usize` (which fits in u64) so we do
    // not rely on transmute-able pointer-vs-integer layout beyond what
    // `as usize` already guarantees.
    let raw = f as usize as u64;
    POLICY_FN.store(raw, Ordering::Release);
}

fn load_policy() -> PolicyFn {
    let raw = POLICY_FN.load(Ordering::Acquire);
    if raw == 0 {
        return default_policy;
    }
    // Safety: only values written via `store_policy` (i.e. valid `PolicyFn`
    // pointers cast through `as usize`) ever reach this load. The
    // `as usize as fn(...)` round-trip is the documented inverse.
    unsafe { std::mem::transmute::<usize, PolicyFn>(raw as usize) }
}

/// Override the active outbound-host policy. The previous policy is
/// dropped (function pointers don't need cleanup).
pub fn set_policy(f: PolicyFn) {
    store_policy(f);
}

/// Reset to the default (link-local-metadata-blocking) policy. Mostly
/// useful for tests that installed a custom policy and want to restore
/// the standard one before tearing down.
pub fn reset_policy() {
    POLICY_FN.store(0, Ordering::Release);
}

/// The default policy: deny well-known cloud-metadata link-local IPs,
/// allow everything else. This matches the AWS, GCP, Azure, Oracle,
/// Alibaba, and DigitalOcean metadata endpoints — all of which sit on
/// either `169.254.169.254` (IPv4 link-local) or `fd00:ec2::254`
/// (IPv6 unique-local, AWS).
///
/// We also block the broader IPv4 link-local range (`169.254.0.0/16`)
/// and IPv6 link-local (`fe80::/10`) when the target is an unresolved
/// hostname that *parses* as one of those. By default we deliberately do
/// NOT do DNS resolution here — that would double the latency of every
/// connect and create a TOCTOU window between the policy check and
/// the actual connect. The connect site itself does the DNS work, so
/// blocking at the literal-IP layer catches the direct-IP SSRF that
/// guest code typically attempts, and `policy_connect` additionally
/// re-vets every *resolved* `SocketAddr` it is about to dial (the
/// always-on, TOCTOU-free DNS-alias defence). Embedders that want
/// resolution-aware policy can install their own via `set_policy`, or
/// opt into in-policy resolution (see below).
///
/// DNS-ALIAS HARDENING (2026-06-21): when the opt-in
/// `CRATONVM_RESOLVE_OUTBOUND_HOST` env flag is engaged AND the target is a
/// hostname (not an IP literal), the default policy resolves the name and
/// applies the same IP block to EVERY resolved address — denying if ANY of
/// them lands in a blocked range. This closes the DNS-alias bypass for
/// callers that invoke `check_outbound` directly without going through
/// `policy_connect`. Off by default (preserves the no-DNS posture above).
///
/// V2 HARDENING (2026-06-10): when the opt-in `CRATONVM_BLOCK_PRIVATE_NETS`
/// env flag is engaged, the default policy *additionally* denies loopback
/// (`127.0.0.0/8`, `::1`) and the RFC1918 private ranges (`10.0.0.0/8`,
/// `172.16.0.0/12`, `192.168.0.0/16`) plus their IPv6 equivalents (IPv4-
/// mapped private ranges and the `fc00::/7` unique-local block), so a
/// confined / untrusted workload cannot reach internal services via SSRF.
/// The flag is **off by default** — the env-less default behaviour (block
/// only link-local cloud-metadata) is unchanged, matching the JDK-faithful
/// permissive default the rest of the crate uses. Embedders that need a
/// different posture still install their own rule via [`set_policy`].
fn default_policy(target: &str) -> PolicyDecision {
    let host = host_part(target);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return classify_ip(&ip);
    }
    // The host is not an IP literal — it's a hostname.
    //
    // DNS-ALIAS HARDENING (2026-06-21): without resolving the name, a
    // hostname that resolves to a metadata / private IP (e.g.
    // `metadata.google.internal` → `169.254.169.254`, or an attacker-
    // controlled name that resolves into RFC1918) bypasses the IP-based
    // block, since the literal-string check above never fires. When the
    // opt-in `CRATONVM_RESOLVE_OUTBOUND_HOST` flag is engaged we resolve
    // the name here and apply the same per-IP block to EVERY resolved
    // address — denying if ANY of them lands in a blocked range.
    //
    // This is **off by default**: the env-less default deliberately does
    // NOT resolve (see the module doc) to avoid doubling connect latency
    // and to avoid a TOCTOU window between the policy check and the actual
    // connect — `policy_connect` already re-vets each *resolved*
    // `SocketAddr` it is about to dial, which is the TOCTOU-free
    // enforcement point. The flag exists so embedders that call
    // `check_outbound` directly (without going through `policy_connect`)
    // can still get resolution-aware blocking from the built-in policy.
    if resolve_outbound_host_enabled() {
        if let Some(decision) = resolve_and_classify_host(host) {
            return decision;
        }
    }
    PolicyDecision::Allow
}

/// Apply the built-in block ranges to a single resolved-or-literal IP.
/// Factored out of `default_policy` so the literal-IP path and the
/// hostname-resolution path (`resolve_and_classify_host`) share one
/// classifier — they must stay byte-for-byte identical so a hostname alias
/// cannot reach anything a literal IP could not.
fn classify_ip(ip: &IpAddr) -> PolicyDecision {
    if is_link_local_metadata_ip(ip) {
        return PolicyDecision::Deny(format!(
            "link-local cloud-metadata address {ip} is blocked by default policy"
        ));
    }
    // Opt-in (CRATONVM_BLOCK_PRIVATE_NETS): also deny loopback + RFC1918
    // private ranges so an untrusted workload can't reach internal
    // services. Default-off; the link-local block above always runs.
    if block_private_nets_enabled() && is_private_or_loopback_ip(ip) {
        return PolicyDecision::Deny(format!(
            "private/loopback address {ip} is blocked (CRATONVM_BLOCK_PRIVATE_NETS)"
        ));
    }
    PolicyDecision::Allow
}

/// Resolve `host` (a bare hostname, no port) and apply [`classify_ip`] to
/// every address it resolves to, returning the first `Deny` — i.e. deny if
/// ANY resolved address is in a blocked range. Returns `None` when nothing
/// is blocked (so the caller falls through to `Allow`), and also `None`
/// when resolution itself fails: a name that does not resolve cannot be
/// connected anyway, and we deliberately do not turn a transient DNS error
/// into a policy denial (the connect site surfaces the real DNS error).
///
/// `host` is appended with a throwaway `:0` port so we can reuse
/// `ToSocketAddrs`, which only resolves `host:port` shapes. The port is
/// irrelevant — we only inspect the resolved `IpAddr`s.
fn resolve_and_classify_host(host: &str) -> Option<PolicyDecision> {
    let resolved = (host, 0u16).to_socket_addrs().ok()?;
    for sa in resolved {
        let ip = sa.ip();
        if let PolicyDecision::Deny(reason) = classify_ip(&ip) {
            return Some(PolicyDecision::Deny(format!(
                "hostname {host} resolves to blocked address {ip}: {reason}"
            )));
        }
    }
    None
}

/// Cached `CRATONVM_RESOLVE_OUTBOUND_HOST` flag. Same tri-state atomic
/// encoding and presence/`0`/`false`/`off`/`no` semantics as
/// `block_private_nets_enabled`. Off by default so the standard policy keeps
/// its no-DNS, low-latency, TOCTOU-free posture (resolution-aware blocking
/// at the literal-IP layer in `policy_connect` is the always-on path).
fn resolve_outbound_host_enabled() -> bool {
    crate::io_flags().resolve_outbound_host
}

/// Cached `CRATONVM_BLOCK_PRIVATE_NETS` flag. Read once on first connect so
/// the policy stays a cheap branch on the hot path (the env var cannot
/// meaningfully change mid-process). Presence = enabled; the value `0` /
/// `false` / `off` / `no` (case-insensitive) disables. Tri-state encoding in
/// the atomic: 0 = not yet computed, 1 = disabled, 2 = enabled — so the
/// "absent" default (disabled) is never mistaken for "uncomputed".
fn block_private_nets_enabled() -> bool {
    crate::io_flags().block_private_nets
}

/// Returns true for loopback and RFC1918 private IPv4/IPv6 addresses.
/// Used only when `CRATONVM_BLOCK_PRIVATE_NETS` is engaged; the link-local
/// metadata block is separate and always-on. Does not overlap the
/// metadata check (link-local `169.254/16` and `fe80::/10` are handled by
/// [`is_link_local_metadata_ip`]).
fn is_private_or_loopback_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_v4_private_or_loopback(v4),
        IpAddr::V6(v6) => {
            // An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) tunnels the v4
            // ranges — classify by the embedded v4 octets so a mapped
            // private/loopback address can't slip past.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_v4_private_or_loopback(&v4);
            }
            is_v6_private_or_loopback(v6)
        }
    }
}

fn is_v4_private_or_loopback(v4: &Ipv4Addr) -> bool {
    let o = v4.octets();
    // 127.0.0.0/8 loopback
    if o[0] == 127 {
        return true;
    }
    // 10.0.0.0/8
    if o[0] == 10 {
        return true;
    }
    // 172.16.0.0/12  (172.16.0.0 – 172.31.255.255)
    if o[0] == 172 && (16..=31).contains(&o[1]) {
        return true;
    }
    // 192.168.0.0/16
    if o[0] == 192 && o[1] == 168 {
        return true;
    }
    false
}

fn is_v6_private_or_loopback(v6: &Ipv6Addr) -> bool {
    // ::1 loopback
    if v6.is_loopback() {
        return true;
    }
    // fc00::/7 — IPv6 unique-local addresses (the v6 RFC1918 equivalent).
    // Top 7 bits == 1111110.
    let segs = v6.segments();
    if segs[0] & 0xfe00 == 0xfc00 {
        return true;
    }
    false
}

/// Strip the `:port` suffix from a `host:port` string and return the bare
/// host. Handles three input shapes:
///
///   * bracketed IPv6 (`[::1]:80` → `::1`, `[fe80::1]` → `fe80::1`),
///   * `host:port` with a single colon (`127.0.0.1:80` → `127.0.0.1`,
///     `example.com:443` → `example.com`), and
///   * a **bare, unbracketed IPv6 literal** (`fe80::1`, `::1`,
///     `fd00:ec2::254`) which contains multiple colons and *no* port.
///
/// B4 FIX (2026-06-10): the old `rsplit_once(':')` split a bare IPv6 literal
/// on the colon *inside* the address (`fe80::1` → `fe80:`), yielding an
/// unparseable host so `default_policy` returned `Allow` — letting an
/// unbracketed link-local / metadata IPv6 literal slip past the default
/// block at the literal-string layer. We now only strip a trailing `:port`
/// when the unbracketed string contains exactly one colon; a string with
/// two or more colons and no brackets is a bare IPv6 literal and is returned
/// whole. (Bracketed forms keep `host:port` parsing unambiguous and are
/// handled first, so a bracketed literal *with* a port is unaffected.)
fn host_part(target: &str) -> &str {
    if let Some(rest) = target.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return &rest[..end];
        }
    }
    // Unbracketed: a single colon is `host:port`; two or more colons is a
    // bare IPv6 literal (no port) and must be returned intact so it parses
    // as an `IpAddr`. `matches(':').count()` counts colon occurrences.
    if target.matches(':').count() >= 2 {
        return target;
    }
    match target.rsplit_once(':') {
        Some((host, _)) => host,
        None => target,
    }
}

/// Returns true for IP addresses known to be cloud-metadata link-local
/// endpoints. We err on the strict side: any address inside the IPv4
/// link-local block (`169.254.0.0/16`) is rejected, since the metadata
/// service routinely uses neighbouring addresses (`169.254.170.2` for
/// ECS task-role credentials, etc.).
fn is_link_local_metadata_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_v4_link_local(v4),
        IpAddr::V6(v6) => {
            // SSRF FIX (2026-06-21): an IPv4-mapped IPv6 literal
            // (`::ffff:a.b.c.d`) tunnels the v4 link-local block. Without
            // unwrapping it here, `::ffff:169.254.169.254` (AWS IMDS) and
            // `::ffff:169.254.170.2` (ECS task-role credentials) parse as
            // `IpAddr::V6`, skip the v4 metadata check, and only reach
            // `is_v6_link_local_or_metadata` — which matches fe80::/10 and
            // `fd00:ec2::254` but NOT the mapped v4 range — so they reached
            // the metadata service. Mirror `is_private_or_loopback_ip`:
            // classify by the embedded v4 octets first. We also fold in the
            // deprecated IPv4-compatible form (`::a.b.c.d`, RFC 4291
            // §2.5.5.1) so that legacy representation can't slip the same
            // block either.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_v4_link_local(&v4);
            }
            if let Some(v4) = v6_to_ipv4_compatible(v6) {
                return is_v4_link_local(&v4);
            }
            is_v6_link_local_or_metadata(v6)
        }
    }
}

/// Unwrap the deprecated IPv4-compatible IPv6 form `::a.b.c.d` (RFC 4291
/// §2.5.5.1): the high 96 bits are zero and the low 32 bits hold an IPv4
/// address. `std` has `to_ipv4_mapped` for `::ffff:a.b.c.d` but no direct
/// accessor for the compatible form (`Ipv6Addr::to_ipv4` conflates the two
/// and also matches `::1`/`::`), so we decode it explicitly here. Returns
/// `None` for the unspecified (`::`) and loopback (`::1`) addresses, which
/// are not link-local v4 tunnels and must fall through to the v6 classifier.
fn v6_to_ipv4_compatible(v6: &Ipv6Addr) -> Option<Ipv4Addr> {
    let segs = v6.segments();
    // High 96 bits (segments 0..=5) must all be zero for the compatible form.
    if segs[0..6].iter().any(|&s| s != 0) {
        return None;
    }
    let v4 = Ipv4Addr::new(
        (segs[6] >> 8) as u8,
        (segs[6] & 0xff) as u8,
        (segs[7] >> 8) as u8,
        (segs[7] & 0xff) as u8,
    );
    // Exclude `::` (0.0.0.0) and `::1` (0.0.0.1) — these are the
    // unspecified / loopback v6 addresses, not embedded v4 link-local
    // tunnels; let the v6 classifier handle them.
    if v4.is_unspecified() || v4 == Ipv4Addr::new(0, 0, 0, 1) {
        return None;
    }
    Some(v4)
}

fn is_v4_link_local(v4: &Ipv4Addr) -> bool {
    // 169.254.0.0/16
    v4.octets()[0] == 169 && v4.octets()[1] == 254
}

fn is_v6_link_local_or_metadata(v6: &Ipv6Addr) -> bool {
    let segs = v6.segments();
    // fe80::/10 — IPv6 link-local
    if segs[0] & 0xffc0 == 0xfe80 {
        return true;
    }
    // fd00:ec2::254 — AWS IPv6 metadata
    if segs[0] == 0xfd00 && segs[1] == 0xec2 && segs[7] == 0x254 {
        return true;
    }
    false
}

/// Evaluate the active policy against `target`. Called from every
/// blocking connect site. Returns `Err(reason)` if the connect must
/// be refused; the caller is responsible for translating that into a
/// Java `IOException`.
pub fn check_outbound(target: &str) -> Result<(), String> {
    match (load_policy())(target) {
        PolicyDecision::Allow => Ok(()),
        PolicyDecision::Deny(reason) => Err(reason),
    }
}

// ---------------------------------------------------------------------------
// Helpers: connect with the configured timeout, honouring the policy.
// ---------------------------------------------------------------------------

/// Collapse an IPv4-mapped IPv6 destination (`::ffff:a.b.c.d`) to the plain
/// IPv4 address before dialling it. Every other address passes through.
///
/// **Why this is not cosmetic.** `TcpStream::connect*` picks the socket family
/// from the `SocketAddr`, so a `SocketAddr::V6` gets an AF_INET6 socket — and
/// on Windows `IPV6_V6ONLY` defaults to **1**, so that socket cannot reach a
/// v4-mapped destination at all: `connect` fails with WSAEADDRNOTAVAIL
/// (`os error 10049`, "the requested address is not valid in its context").
/// Linux defaults the option off (`net.ipv6.bindv6only=0`), which is why the
/// same code path works there and this is a Windows-only failure.
///
/// Real JDK never hands the OS this destination in the first place:
/// `InetAddress.getByName("::ffff:127.0.0.1")` returns an **`Inet4Address`**,
/// so the JDK builds an AF_INET socket to `127.0.0.1`. CratonVM's own
/// `InetAddress` layer already mirrors that fold (`net_phase_e`'s
/// `hotspot_ip_string`) — but any connect path that re-parses the destination
/// from a *string* in Rust (a URL's host, or an `InetSocketAddress` that kept
/// its original hostname text) bypasses it and inherits the platform
/// behaviour. That is how `TestStartupIPv6Connectors.testIPv6MappedIPv4`
/// failed, via `HttpURLConnection`, and `SocketChannel.connect` with it.
///
/// Same spirit as `socket_channel::connect_target_host`'s wildcard→loopback
/// rewrite: normalise a destination the OS was never meant to receive, rather
/// than handing it over and reporting the OS's complaint.
///
/// Only the *mapped* form (`::ffff:a.b.c.d`) is folded, matching
/// `hotspot_ip_string` exactly. `Ipv6Addr::to_ipv4` is deliberately NOT used:
/// it also matches the deprecated IPv4-compatible form and `::1`, so `::1`
/// would silently become `0.0.0.1`.
pub fn normalize_connect_addr(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::from((v4, v6.port())),
            None => addr,
        },
        _ => addr,
    }
}

/// `TcpStream::connect(target)` with [`normalize_connect_addr`] applied to
/// every resolved candidate.
///
/// Drop-in replacement for `TcpStream::connect(&host_port_string)` at sites
/// that dial a host taken from a URL or config. `TcpStream::connect(&str)`
/// resolves and iterates internally, so there is no way to fold the addresses
/// without taking the resolution over — hence this helper rather than a
/// one-line `.map()` like the sites that already resolve for themselves.
///
/// No policy check: this exists purely for the address fold, and the callers
/// are paths that historically did not consult the outbound policy. Use
/// [`policy_connect`] when the policy should apply.
pub fn connect_str_normalized(target: &str) -> std::io::Result<std::net::TcpStream> {
    let mut last_err: Option<std::io::Error> = None;
    for addr in target.to_socket_addrs()?.map(normalize_connect_addr) {
        match std::net::TcpStream::connect(addr) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            format!("no addresses resolved for {target}"),
        )
    }))
}

/// Connect to `target` (a `host:port` string) with the policy + timeout
/// applied. On policy reject the caller gets `Err` with an
/// "outbound policy" message; on connect error / DNS failure / timeout
/// the original `io::Error` is returned so the call site can map it
/// through its existing error translator.
///
/// We resolve `target` to one or more `SocketAddr`s (so we can use
/// `connect_timeout`), then iterate exactly like
/// `TcpStream::connect(&str)` would, but with the timeout applied to
/// every candidate. The first `Ok` wins; the last error is returned
/// otherwise.
pub fn policy_connect(target: &str) -> Result<std::net::TcpStream, PolicyConnectError> {
    policy_connect_with(target, None)
}

/// [`policy_connect`], but dialling an address the CALLER already resolved.
///
/// # Why this exists
///
/// `SocketChannel.connect(new InetSocketAddress(addr, port))` hands us a
/// destination that is *already resolved* — `InetSocketAddress` holds an
/// `InetAddress`. CratonVM used to throw that away: `decode_socket_address`
/// reads the target with `getHostString()`, which yields the HOSTNAME whenever
/// one is attached, so every dial re-entered `to_socket_addrs` and ran a fresh
/// `getaddrinfo`. HotSpot performs no such lookup.
///
/// That cost a DNS round trip per connection on the VM's busiest connect path,
/// and on Windows it also HUNG: the resolver stops returning after a few dozen
/// rapid `localhost` lookups, and because the block is inside resolution rather
/// than the dial, [`connect_timeout`]'s finite cap never applies. See
/// `known-issues/netty/blocking-connect-accept-stalls-near-128-connections-20260905.md`.
///
/// # What is preserved, and why it is not just a literal substitution
///
/// The obvious fix — hand the literal IP to `policy_connect` instead of the
/// name — would be a **security regression**. `check_outbound` is called on the
/// target FIRST precisely so a name-based policy can refuse by name; the worked
/// example in this module is `metadata.google.internal`. Substituting the
/// literal at the call site would leave that policy never seeing the name.
///
/// So the NAME is still vetted here exactly as before, and only the RESOLUTION
/// step is skipped. The per-address re-check below still runs against the
/// pre-resolved address, so an embedder's policy gets both halves.
///
/// Skipping the re-resolution is also a small SSRF improvement in its own
/// right: resolving a second time is a genuine TOCTOU between the address the
/// caller vetted and the one we dial, which is the DNS-rebind window this
/// module's own comments describe.
pub fn policy_connect_with(
    target: &str,
    preresolved: Option<SocketAddr>,
) -> Result<std::net::TcpStream, PolicyConnectError> {
    // The NAME policy, unchanged and still first.
    check_outbound(target).map_err(PolicyConnectError::Denied)?;

    // Resolution: do it ourselves so we have a `SocketAddr` to feed to
    // `connect_timeout`. If the input is already a literal `IP:port`,
    // `to_socket_addrs` short-circuits without DNS lookup.
    // Normalise BEFORE the per-address policy re-check below, so the address
    // that gets vetted is the one that actually gets dialled.
    let addrs: Vec<SocketAddr> = match preresolved {
        Some(addr) => vec![normalize_connect_addr(addr)],
        None => target
            .to_socket_addrs()
            .map_err(PolicyConnectError::Io)?
            .map(normalize_connect_addr)
            .collect(),
    };

    if addrs.is_empty() {
        return Err(PolicyConnectError::Io(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            format!("no addresses resolved for {target}"),
        )));
    }

    let timeout = connect_timeout();
    let mut last_err: Option<std::io::Error> = None;
    for addr in addrs {
        // Re-check the *active* policy against the resolved IP — this
        // closes the hostname-aliasing / DNS-rebind escape (e.g.
        // `metadata.google.internal` resolves to `169.254.169.254`, or a
        // custom RFC1918-denying policy bypassed by a hostname that
        // resolves to a private IP). We re-invoke `check_outbound` per
        // resolved address rather than hardcoding the built-in link-local
        // ranges, so an embedder's `set_policy` is enforced here too. This
        // mirrors `socket_channel.rs::resolve_and_vet` so both the blocking
        // and non-blocking connect paths stay consistent. The built-in
        // link-local block is subsumed by the default policy, so default
        // behaviour is unchanged. A bracketed literal keeps IPv6
        // `host:port` parsing unambiguous, matching `host_part`.
        let literal = match addr {
            SocketAddr::V4(_) => format!("{}:{}", addr.ip(), addr.port()),
            SocketAddr::V6(_) => format!("[{}]:{}", addr.ip(), addr.port()),
        };
        if let Err(reason) = check_outbound(&literal) {
            return Err(PolicyConnectError::Denied(format!(
                "resolved address {} of {target} is blocked: {reason}",
                addr.ip()
            )));
        }
        match std::net::TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = Some(e),
        }
    }
    Err(PolicyConnectError::Io(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "policy_connect: no addresses tried",
        )
    })))
}

/// Error type for `policy_connect`. The two variants let the caller
/// pick the right Java exception: policy denials map to a plain
/// `IOException`, while I/O errors flow through the existing
/// `ConnectException` / `SocketTimeoutException` translator.
#[derive(Debug)]
pub enum PolicyConnectError {
    Denied(String),
    Io(std::io::Error),
}

impl std::fmt::Display for PolicyConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyConnectError::Denied(r) => write!(f, "connect denied by outbound policy: {r}"),
            PolicyConnectError::Io(e) => write!(f, "{e}"),
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
    use std::net::TcpListener;
    use std::sync::Mutex;

    // The policy + timeout cells are process-global. The tests that
    // mutate them must serialize so a parallel run doesn't see another
    // test's override.
    static MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn rejects_aws_imds_v4() {
        let _g = MUTEX.lock().unwrap();
        reset_policy();
        let res = policy_connect("169.254.169.254:80");
        match res {
            Err(PolicyConnectError::Denied(msg)) => {
                assert!(msg.contains("169.254"), "msg={msg}");
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn rejects_aws_imds_v6() {
        let _g = MUTEX.lock().unwrap();
        reset_policy();
        let res = policy_connect("[fd00:ec2::254]:80");
        assert!(matches!(res, Err(PolicyConnectError::Denied(_))));
    }

    #[test]
    fn rejects_v4_link_local_neighbour() {
        // ECS task-role-credentials endpoint sits at 169.254.170.2,
        // *not* .169.254 — the policy must catch the whole /16.
        let _g = MUTEX.lock().unwrap();
        reset_policy();
        let res = policy_connect("169.254.170.2:80");
        assert!(matches!(res, Err(PolicyConnectError::Denied(_))));
    }

    #[test]
    fn times_out_on_blackhole_test_net() {
        // RFC 5737 TEST-NET-1 is reserved; nothing should ever answer.
        let _g = MUTEX.lock().unwrap();
        reset_policy();
        set_connect_timeout(Duration::from_millis(200));
        let start = std::time::Instant::now();
        let res = policy_connect("192.0.2.1:80");
        let elapsed = start.elapsed();
        // Reset timeout for other tests.
        set_connect_timeout(Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS));
        // Some platforms return AddrNotAvailable / network-unreachable
        // immediately instead of timing out; we accept either outcome
        // so the test isn't flaky on CI runners with restrictive
        // egress. What we DO require is that we didn't sit there for
        // the OS default (~75 s on Linux, longer on Windows).
        match res {
            Err(PolicyConnectError::Io(e)) => {
                assert!(
                    elapsed < Duration::from_secs(5),
                    "took {:?} which is longer than the configured 200 ms cap: err={e}",
                    elapsed
                );
            }
            Err(PolicyConnectError::Denied(d)) => panic!("unexpected denial: {d}"),
            Ok(_) => panic!("blackhole connect somehow succeeded"),
        }
    }

    #[test]
    fn allows_localhost_by_default() {
        let _g = MUTEX.lock().unwrap();
        reset_policy();
        // Start a real listener so the connect actually succeeds.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let target = format!("127.0.0.1:{port}");
        let _accept_thread = std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let res = policy_connect(&target);
        assert!(
            res.is_ok(),
            "default policy should allow localhost; got {res:?}"
        );
    }

    #[test]
    fn custom_policy_overrides_default() {
        let _g = MUTEX.lock().unwrap();
        // Custom policy: deny everything.
        fn deny_all(_t: &str) -> PolicyDecision {
            PolicyDecision::Deny("test policy denies all".to_string())
        }
        set_policy(deny_all);
        let res = policy_connect("127.0.0.1:9");
        assert!(matches!(res, Err(PolicyConnectError::Denied(_))));
        // And restore the default so we don't poison other tests.
        reset_policy();
        // And confirm the default came back.
        // (We can't reach a real host without networking; rely on the
        // localhost test above for that side. Here we just verify the
        // policy is no longer denying 127.0.0.1.)
        match (load_policy())("127.0.0.1:1") {
            PolicyDecision::Allow => {}
            other => panic!("expected default Allow after reset, got {other:?}"),
        }
    }

    #[test]
    fn connect_timeout_round_trips_through_setter() {
        let _g = MUTEX.lock().unwrap();
        let original = connect_timeout();
        set_connect_timeout(Duration::from_millis(1234));
        assert_eq!(connect_timeout(), Duration::from_millis(1234));
        // ZERO acts as "reset to default" (still finite).
        set_connect_timeout(Duration::ZERO);
        assert_eq!(
            connect_timeout(),
            Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS)
        );
        // Restore.
        set_connect_timeout(original);
    }

    #[test]
    fn host_part_strips_brackets_and_port() {
        assert_eq!(host_part("127.0.0.1:80"), "127.0.0.1");
        assert_eq!(host_part("[::1]:80"), "::1");
        assert_eq!(host_part("example.com:443"), "example.com");
        assert_eq!(host_part("bare"), "bare");
    }

    /// B4 (2026-06-10): a bare, *unbracketed* IPv6 literal (multiple colons,
    /// no port) must be returned whole so it parses as an `IpAddr`. The old
    /// `rsplit_once(':')` split it on an internal colon (`fe80::1` → `fe80:`),
    /// making it unparseable and silently `Allow`ed by `default_policy`.
    #[test]
    fn host_part_handles_bare_ipv6_literal() {
        assert_eq!(host_part("fe80::1"), "fe80::1");
        assert_eq!(host_part("::1"), "::1");
        assert_eq!(host_part("fd00:ec2::254"), "fd00:ec2::254");
        // Bracketed literal *without* a port still strips the brackets.
        assert_eq!(host_part("[fe80::1]"), "fe80::1");
        // And each of these now parses as a real IpAddr (the whole point).
        assert!(host_part("fe80::1").parse::<IpAddr>().is_ok());
        assert!(host_part("fd00:ec2::254").parse::<IpAddr>().is_ok());
    }

    /// B4 end-to-end: an unbracketed link-local / metadata IPv6 literal must
    /// be denied by the default policy (previously slipped through because
    /// `host_part` mangled it). `default_policy` is consulted directly so the
    /// test does not depend on networking.
    #[test]
    fn default_policy_denies_bare_ipv6_metadata() {
        // Unbracketed link-local and AWS-metadata IPv6 literals — these are
        // exactly the inputs the old `host_part` mangled into `Allow`.
        assert!(matches!(default_policy("fe80::1"), PolicyDecision::Deny(_)));
        assert!(matches!(
            default_policy("fd00:ec2::254"),
            PolicyDecision::Deny(_)
        ));
        // The bracketed-with-port form must keep working too.
        assert!(matches!(
            default_policy("[fd00:ec2::254]:80"),
            PolicyDecision::Deny(_)
        ));
        // A public IPv6 literal is still allowed (no false positives).
        assert!(matches!(
            default_policy("2001:4860:4860::8888"),
            PolicyDecision::Allow
        ));
    }

    /// V2 (2026-06-10): the opt-in private-net classifier covers loopback
    /// (`127/8`, `::1`) and the RFC1918 ranges (`10/8`, `172.16/12`,
    /// `192.168/16`) plus IPv6 ULA (`fc00::/7`) and IPv4-mapped private v6.
    /// These functions are gated on `CRATONVM_BLOCK_PRIVATE_NETS` in
    /// `default_policy`; here we test the pure classifier so the assertion
    /// does not race the process-global env cache.
    #[test]
    fn private_net_classifier_covers_loopback_and_rfc1918() {
        let denied = [
            "127.0.0.1",
            "127.255.255.255",
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "::ffff:10.0.0.1", // IPv4-mapped private v6
            "::ffff:127.0.0.1",
        ];
        for s in denied {
            let ip: IpAddr = s.parse().unwrap();
            assert!(
                is_private_or_loopback_ip(&ip),
                "expected private/loopback: {s}"
            );
        }
        let allowed = [
            "8.8.8.8",
            "1.1.1.1",
            "172.15.0.1", // just below the /12
            "172.32.0.1", // just above the /12
            "192.169.0.1",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8", // IPv4-mapped public v6
        ];
        for s in allowed {
            let ip: IpAddr = s.parse().unwrap();
            assert!(!is_private_or_loopback_ip(&ip), "expected public: {s}");
        }
    }

    /// The private-net block must NOT overlap or weaken the always-on
    /// link-local metadata block: link-local addresses stay denied
    /// regardless of the opt-in flag.
    #[test]
    fn private_net_classifier_excludes_link_local_metadata() {
        // 169.254/16 and fe80::/10 are handled by the metadata classifier,
        // not the private classifier — verify they are not mis-bucketed.
        let imds: IpAddr = "169.254.169.254".parse().unwrap();
        assert!(!is_private_or_loopback_ip(&imds));
        assert!(is_link_local_metadata_ip(&imds));
    }

    /// SSRF FIX (2026-06-21): IPv4-mapped IPv6 literals must NOT bypass the
    /// always-on cloud-metadata / link-local block. `::ffff:169.254.169.254`
    /// (AWS IMDS) and `::ffff:169.254.170.2` (ECS task-role creds) parse as
    /// `IpAddr::V6` but tunnel a v4 link-local address — they previously
    /// skipped the v4 check and were allowed through.
    #[test]
    fn link_local_block_unwraps_ipv4_mapped_v6() {
        let blocked = [
            "::ffff:169.254.169.254", // AWS / GCP / Azure IMDS
            "::ffff:169.254.170.2",   // ECS task-role credentials
            "::ffff:169.254.0.0",     // bottom of the /16
            "::ffff:169.254.255.255", // top of the /16
        ];
        for s in blocked {
            let ip: IpAddr = s.parse().unwrap();
            assert!(
                is_link_local_metadata_ip(&ip),
                "IPv4-mapped link-local must be blocked: {s}"
            );
        }
        // A mapped *public* v4 address must still be allowed (no false
        // positive that would break legitimate IPv4-mapped connects).
        let public_mapped: IpAddr = "::ffff:8.8.8.8".parse().unwrap();
        assert!(!is_link_local_metadata_ip(&public_mapped));
    }

    /// The deprecated IPv4-compatible form `::a.b.c.d` (RFC 4291 §2.5.5.1)
    /// must also be unwrapped — otherwise it is a second tunnel past the v4
    /// link-local block. `::` and `::1` must NOT be misread as embedded v4.
    #[test]
    fn link_local_block_unwraps_ipv4_compatible_v6() {
        // `::169.254.169.254` — note Rust formats embedded-v4 zeros-high as a
        // bare v6 literal, so we build it from segments to be explicit.
        let imds_compat = Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0xa9fe, 0xa9fe);
        assert_eq!(
            v6_to_ipv4_compatible(&imds_compat),
            Some(Ipv4Addr::new(169, 254, 169, 254))
        );
        assert!(is_link_local_metadata_ip(&IpAddr::V6(imds_compat)));

        // A compatible *public* v4 (e.g. 8.8.8.8 == 0x0808:0808) is allowed.
        let public_compat = Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0x0808, 0x0808);
        assert_eq!(
            v6_to_ipv4_compatible(&public_compat),
            Some(Ipv4Addr::new(8, 8, 8, 8))
        );
        assert!(!is_link_local_metadata_ip(&IpAddr::V6(public_compat)));

        // `::` (unspecified) and `::1` (loopback) are not v4 tunnels.
        assert_eq!(v6_to_ipv4_compatible(&Ipv6Addr::UNSPECIFIED), None);
        assert_eq!(v6_to_ipv4_compatible(&Ipv6Addr::LOCALHOST), None);
        // And they must not be classified as link-local metadata.
        assert!(!is_link_local_metadata_ip(&IpAddr::V6(
            Ipv6Addr::UNSPECIFIED
        )));
        assert!(!is_link_local_metadata_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    /// End-to-end: the default policy denies an IPv4-mapped IMDS literal in
    /// both bracketed-with-port and bare forms — exactly the inputs that
    /// previously slipped through the always-on block.
    #[test]
    fn default_policy_denies_ipv4_mapped_metadata() {
        assert!(matches!(
            default_policy("[::ffff:169.254.169.254]:80"),
            PolicyDecision::Deny(_)
        ));
        assert!(matches!(
            default_policy("::ffff:169.254.170.2"),
            PolicyDecision::Deny(_)
        ));
    }

    /// DNS-ALIAS (2026-06-21): the per-IP classifier shared by the literal
    /// and resolution paths must give identical verdicts — a hostname alias
    /// can never reach anything a literal IP could not. Tested directly so
    /// the assertion does not race the process-global env caches.
    #[test]
    fn classify_ip_matches_literal_policy_for_metadata() {
        let imds: IpAddr = "169.254.169.254".parse().unwrap();
        assert!(matches!(classify_ip(&imds), PolicyDecision::Deny(_)));
        let ecs: IpAddr = "169.254.170.2".parse().unwrap();
        assert!(matches!(classify_ip(&ecs), PolicyDecision::Deny(_)));
        // IPv4-mapped metadata is blocked through the shared classifier too.
        let mapped: IpAddr = "::ffff:169.254.169.254".parse().unwrap();
        assert!(matches!(classify_ip(&mapped), PolicyDecision::Deny(_)));
        // A public address passes (no false positives).
        let public: IpAddr = "8.8.8.8".parse().unwrap();
        assert!(matches!(classify_ip(&public), PolicyDecision::Allow));
    }

    /// DNS-ALIAS resolution plumbing, exercised offline: a *literal* IP
    /// passed as the "host" string is resolved by `ToSocketAddrs` WITHOUT a
    /// DNS query (it short-circuits), so we can assert the resolve+classify
    /// machinery blocks a metadata address and passes a public one without
    /// touching the network. This is the same path a real hostname that
    /// resolves to these addresses would take.
    #[test]
    fn resolve_and_classify_blocks_metadata_offline() {
        // 169.254.169.254 is always blocked (link-local metadata, no flag).
        match resolve_and_classify_host("169.254.169.254") {
            Some(PolicyDecision::Deny(msg)) => {
                assert!(msg.contains("resolves to blocked address"), "msg={msg}");
                assert!(msg.contains("169.254.169.254"), "msg={msg}");
            }
            other => panic!("expected Deny for IMDS, got {other:?}"),
        }
        // A public literal resolves but is not blocked → None (caller Allows).
        assert_eq!(resolve_and_classify_host("8.8.8.8"), None);
        // A name that cannot resolve must not become a policy denial — the
        // connect site surfaces the real DNS error instead.
        assert_eq!(
            resolve_and_classify_host("invalid.invalid.this-tld-does-not-exist"),
            None
        );
    }

    /// IPv4-mapped metadata must also be caught on the resolution path (not
    /// just the literal path) — `::ffff:169.254.169.254` as a resolved
    /// address is blocked via the shared `classify_ip`. Exercised offline by
    /// passing the literal v6 string (no DNS query).
    #[test]
    fn resolve_and_classify_blocks_ipv4_mapped_metadata_offline() {
        match resolve_and_classify_host("::ffff:169.254.169.254") {
            Some(PolicyDecision::Deny(_)) => {}
            other => panic!("expected Deny for mapped IMDS, got {other:?}"),
        }
    }

    /// The `CRATONVM_RESOLVE_OUTBOUND_HOST` flag parses presence and the
    /// `0`/`false`/`off`/`no` disablers identically to the private-nets flag.
    /// We can't flip the cached process-global atomic per-test, so assert the
    /// parsing predicate directly via the same logic the cache uses.
    #[test]
    fn resolve_outbound_flag_parsing_semantics() {
        // Mirror the disabler set the cache uses; presence (non-disabler) = on.
        let is_on = |v: &str| {
            let v = v.trim().to_ascii_lowercase();
            !(v.is_empty() || v == "0" || v == "false" || v == "off" || v == "no")
        };
        for off in ["", " ", "0", "false", "FALSE", "Off", "no", "NO"] {
            assert!(!is_on(off), "expected disabled for {off:?}");
        }
        for on in ["1", "true", "yes", "on", "anything"] {
            assert!(is_on(on), "expected enabled for {on:?}");
        }
    }

    /// An IPv4-mapped destination must be dialled as plain IPv4, because on
    /// Windows an AF_INET6 socket cannot reach one (`IPV6_V6ONLY` defaults to
    /// 1 → WSAEADDRNOTAVAIL). The port must survive the fold.
    #[test]
    fn normalize_connect_addr_folds_v4_mapped() {
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:8080".parse().unwrap();
        let got = normalize_connect_addr(mapped);
        assert!(matches!(got, SocketAddr::V4(_)), "expected V4, got {got}");
        assert_eq!(got.ip().to_string(), "127.0.0.1");
        assert_eq!(got.port(), 8080);

        // A non-loopback mapped address folds the same way.
        let mapped: SocketAddr = "[::ffff:10.1.2.3]:1".parse().unwrap();
        assert_eq!(normalize_connect_addr(mapped).ip().to_string(), "10.1.2.3");
    }

    /// Everything that is NOT `::ffff:a.b.c.d` must pass through untouched.
    /// `::1` is the one that matters: `Ipv6Addr::to_ipv4` (which this must not
    /// use) also matches the loopback and the deprecated IPv4-compatible form,
    /// so it would silently rewrite `::1` to `0.0.0.1` and dial a stranger.
    #[test]
    fn normalize_connect_addr_leaves_everything_else_alone() {
        for literal in [
            "[::1]:80",
            "[fe80::1]:80",
            "[2001:db8::1]:80",
            "[::]:80",
            // Deprecated IPv4-COMPATIBLE form (`::a.b.c.d`), not the mapped
            // one — `hotspot_ip_string` does not fold it either, so neither
            // does this, and the two stay consistent.
            "[::127.0.0.1]:80",
            "127.0.0.1:80",
        ] {
            let addr: SocketAddr = literal.parse().unwrap();
            assert_eq!(
                normalize_connect_addr(addr),
                addr,
                "{literal} must pass through unchanged"
            );
        }
    }
}
