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
/// hostname that *parses* as one of those. We deliberately do NOT do
/// DNS resolution here — that would double the latency of every
/// connect and create a TOCTOU window between the policy check and
/// the actual connect. The connect site itself does the DNS work, so
/// blocking at the literal-IP layer catches the direct-IP SSRF that
/// guest code typically attempts; embedders that want resolution-aware
/// policy can install their own via `set_policy`.
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
        if is_link_local_metadata_ip(&ip) {
            return PolicyDecision::Deny(format!(
                "link-local cloud-metadata address {ip} is blocked by default policy"
            ));
        }
        // Opt-in (CRATONVM_BLOCK_PRIVATE_NETS): also deny loopback + RFC1918
        // private ranges so an untrusted workload can't reach internal
        // services. Default-off; the link-local block above always runs.
        if block_private_nets_enabled() && is_private_or_loopback_ip(&ip) {
            return PolicyDecision::Deny(format!(
                "private/loopback address {ip} is blocked (CRATONVM_BLOCK_PRIVATE_NETS)"
            ));
        }
    }
    PolicyDecision::Allow
}

/// Cached `CRATONVM_BLOCK_PRIVATE_NETS` flag. Read once on first connect so
/// the policy stays a cheap branch on the hot path (the env var cannot
/// meaningfully change mid-process). Presence = enabled; the value `0` /
/// `false` / `off` / `no` (case-insensitive) disables. Tri-state encoding in
/// the atomic: 0 = not yet computed, 1 = disabled, 2 = enabled — so the
/// "absent" default (disabled) is never mistaken for "uncomputed".
static BLOCK_PRIVATE_NETS: AtomicU64 = AtomicU64::new(0);

fn block_private_nets_enabled() -> bool {
    match BLOCK_PRIVATE_NETS.load(Ordering::Relaxed) {
        2 => true,
        1 => false,
        _ => {
            let on = match std::env::var("CRATONVM_BLOCK_PRIVATE_NETS") {
                Ok(v) => {
                    let v = v.trim().to_ascii_lowercase();
                    !(v.is_empty() || v == "0" || v == "false" || v == "off" || v == "no")
                }
                Err(_) => false,
            };
            BLOCK_PRIVATE_NETS.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
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
        IpAddr::V6(v6) => is_v6_link_local_or_metadata(v6),
    }
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
    check_outbound(target).map_err(PolicyConnectError::Denied)?;

    // Resolution: do it ourselves so we have a `SocketAddr` to feed to
    // `connect_timeout`. If the input is already a literal `IP:port`,
    // `to_socket_addrs` short-circuits without DNS lookup.
    let addrs: Vec<SocketAddr> = target
        .to_socket_addrs()
        .map_err(PolicyConnectError::Io)?
        .collect();

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
}
