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
fn default_policy(target: &str) -> PolicyDecision {
    let host = host_part(target);
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_link_local_metadata_ip(&ip) {
            return PolicyDecision::Deny(format!(
                "link-local cloud-metadata address {ip} is blocked by default policy"
            ));
        }
    }
    PolicyDecision::Allow
}

/// Strip the `:port` suffix from a `host:port` string. Handles bracketed
/// IPv6 (`[::1]:80`) too.
fn host_part(target: &str) -> &str {
    if let Some(rest) = target.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return &rest[..end];
        }
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
        // Re-check policy against the *resolved* IP — this closes the
        // hostname-aliasing escape (`metadata.google.internal` resolves
        // to `169.254.169.254`).
        if let IpAddr::V4(v4) = addr.ip() {
            if is_v4_link_local(&v4) {
                return Err(PolicyConnectError::Denied(format!(
                    "resolved address {v4} is link-local; blocked by default policy"
                )));
            }
        } else if let IpAddr::V6(v6) = addr.ip() {
            if is_v6_link_local_or_metadata(&v6) {
                return Err(PolicyConnectError::Denied(format!(
                    "resolved address {v6} is link-local; blocked by default policy"
                )));
            }
        }
        match std::net::TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = Some(e),
        }
    }
    Err(PolicyConnectError::Io(last_err.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "policy_connect: no addresses tried")
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
        assert!(res.is_ok(), "default policy should allow localhost; got {res:?}");
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
        assert_eq!(connect_timeout(), Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS));
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
}
