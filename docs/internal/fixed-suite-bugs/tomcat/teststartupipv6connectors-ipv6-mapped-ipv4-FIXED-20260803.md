# `TestStartupIPv6Connectors.testIPv6MappedIPv4` — connect to `::ffff:127.0.0.1` — FIXED 2026-08-03

| | |
|---|---|
| **Status** | ✅ **FIXED** — real CratonVM defect, on **six** connect paths (the report named one) |
| **HotSpot** | PASS 4/4 (re-verified 2026-08-03) |
| **CratonVM** | was FAIL 3/4 → now **PASS 4/4**, JIT on and off |
| **Filed** | 2026-08-03, rerunning the 07-31 4-shard FAIL/HANG set after merging `dev` (`c1fe51a24`) |
| **Closed** | 2026-08-03, branch `fix/tomcat-ipv6-mapped-ipv4-20260803` |

## Symptom

```
1) testIPv6MappedIPv4(org.apache.catalina.startup.TestStartupIPv6Connectors)
java.io.IOException: HttpURLConnection response failed: connect [::ffff:127.0.0.1]:54718:
<the requested address is not valid in its context> (os error 10049)
```

`os error 10049` is Windows `WSAEADDRNOTAVAIL`.

## Root cause

`TcpStream::connect*` picks the socket family from the `SocketAddr` it is
handed, so a `SocketAddr::V6` gets an **AF_INET6** socket. On Windows
`IPV6_V6ONLY` defaults to **1**, so that socket cannot reach an IPv4-mapped
destination at all — hence `WSAEADDRNOTAVAIL`. Linux defaults the option off
(`net.ipv6.bindv6only=0`), which is why the same code works there and this
never showed up on the Azure runs.

Real JDK never hands the OS that destination in the first place:
`InetAddress.getByName("::ffff:127.0.0.1")` returns an **`Inet4Address`**, so
the JDK opens an AF_INET socket to `127.0.0.1`. The probe confirms it, and
CratonVM's own `InetAddress` layer already mirrors the fold
(`net_phase_e::hotspot_ip_string`).

**The gap is every connect path that re-parses the destination from a *string*
in Rust, bypassing `InetAddress` entirely** — a URL's host text, or an
`InetSocketAddress` that kept the hostname it was constructed with.

The original doc's guess ("`native-io`'s socket-channel layer likely does not
translate ... e.g. failing to set `IPV6_V6ONLY`") was directionally right about
the mechanism but wrong about the remedy: setting `IPV6_V6ONLY=0` would change
dual-stack behaviour VM-wide. Matching the JDK — collapse the address, never
build the v6 socket — is both narrower and what every caller already expects.

## Scope: the report named one path, a probe found six

`Ipv6MappedProbe` (new, `apps/tomcat-suite-runner/probes/`) drives four
connect surfaces against a loopback listener, each with a `127.0.0.1` control
so a red means "this path mishandles the mapped form", not "the network is
down". On the pre-fix binary:

| surface | before | note |
|---|---|---|
| `InetAddress.getByName` | PASS | already folds to `Inet4Address` |
| `java.net.Socket.connect` | PASS | routes via `policy_connect` |
| `SocketChannel.connect` | **FAIL** 10049 | **not mentioned in the report** |
| `HttpURLConnection` | **FAIL** 10049 | the reported failure |

Fixed at every site that dials text, via a new
`native-io::outbound_policy::normalize_connect_addr`:

1. `http_url_connection::perform` — the live `HttpURLConnection` path.
2. `http_url_connection::connect_plain` — its **duplicate** for the pooled
   path (the file says so explicitly: "kept as a separate, small, duplicated
   function").
3. `socket_channel::resolve_and_vet` — non-blocking `SocketChannel`.
4. `outbound_policy::policy_connect` — blocking `SocketChannel`, and
   `Net.connect0` (plain `Socket`).
5. `t27_tls::rustls_client_connect` and `x509_manager`'s OCSP responder dial —
   both used bare `TcpStream::connect(&host_port)`.
6. `http2`'s client connect, which had no resolve loop at all.
7. (`http_client::open_connection` too — see the trap below.)

> **Trap worth remembering.** The first pass fixed `http_client::open_connection`
> and the probe still showed `HttpURLConnection` red. `HttpURLConnection` does
> not use `http_client`; `http_url_connection.rs` carries its own connect loop,
> twice. Textbook "the native is implemented twice — you patched the dead copy".
> Only the probe caught it; the class-level test would have too, but the probe
> said *which* surface in one run.

In `policy_connect` and `resolve_and_vet` the fold runs **before** the
per-address outbound-policy re-check, so the address that gets vetted is the
address that gets dialled — otherwise policy could clear one address and the
process dial another.

`Ipv6Addr::to_ipv4_mapped` deliberately, never `to_ipv4`: the latter also
matches `::1` and the deprecated IPv4-compatible form, so it would silently
rewrite `::1` to `0.0.0.1`. Unit-tested in both directions, and kept consistent
with `hotspot_ip_string`, which folds the mapped form only.

Side benefit: `http_url_connection`'s IPv4-first candidate sort now means what
it says. A mapped address is a v4 destination wearing a v6 sockaddr, so it used
to sort **last** — behind the `[::1]` attempt the sort exists to avoid.

## Verification

Windows 11, `apps/tomcat` fixture. CratonVM binaries built from this branch:
`cratonvm-ipv6mapped-BASELINE-20260803.exe` (parent commit) vs
`cratonvm-ipv6mapped-FIX2-20260803.exe`.

**The reported class**

| | HotSpot | CratonVM before | CratonVM after |
|---|---|---|---|
| `TestStartupIPv6Connectors` | OK (4 tests), 1.2 s | **Tests run: 4, Failures: 1** | **OK (4 tests)**, 5.8 s JIT / 17.3 s `--nojit` |

**`Ipv6MappedProbe`** — 5/7 before, **7/7 after**, identical to HotSpot's 7/7.

**A/B regression sweep** over consumers of every touched file — same 16 classes
on both binaries, so classes that are red for their own reasons cannot read as
regressions:

| class | baseline | fixed |
|---|---|---|
| `catalina.startup.TestStartupIPv6Connectors` | FAIL (6s) | **PASS (10s)** ← the fix |
| `tomcat.integration.httpd.TestBasicProxy` | PASS (8s) | PASS (12s) |
| `tomcat.integration.httpd.TestErrorHandling` | PASS (11s) | PASS (18s) |
| `tomcat.integration.httpd.TestFullReverseProxy` | PASS (8s) | PASS (13s) |
| `tomcat.integration.httpd.TestLargePayloadWithProxy` | PASS (15s) | PASS (20s) |
| `tomcat.integration.httpd.TestRemoteIpValveWithProxy` | PASS (10s) | PASS (13s) |
| `tomcat.integration.httpd.TestSSLValveWithProxy01` | PASS (11s) | PASS (13s) |
| `tomcat.integration.httpd.TestSSLValveWithProxy02` | PASS (12s) | PASS (13s) |
| `tomcat.integration.httpd.TestSessionWithProxy` | PASS (14s) | PASS (17s) |
| `tomcat.util.net.TestClientCert` | FAIL (13s) | FAIL (17s) |
| `tomcat.util.net.TestCustomSslTrustManager` | PASS (9s) | PASS (14s) |
| `tomcat.util.net.TestSsl` | HANG (400s) | HANG (400s) |
| `tomcat.util.net.TestXxxEndpoint` | PASS (43s) | PASS (38s) |
| `tomcat.util.net.ocsp.TestOcspEnabled` | PASS (42s) | PASS (41s) |
| `tomcat.util.net.ocsp.TestOcspSoftFail` | PASS (19s) | PASS (19s) |
| `tomcat.util.net.ocsp.TestOcspTimeout` | PASS (100s) | PASS (100s) |

**Regressions (baseline PASS → fixed not PASS): NONE.** The only row that
changed is the target class. `TestClientCert` and `TestSsl` are red on **both**
arms and are pre-existing: `TestClientCert.testClientCertPostZero` needs real
renegotiation (by design — rustls omits it as its CVE-2009-3555 mitigation),
and `TestSsl`'s client-initiated-renegotiation case is the same story, tracked
in `testssl-client-initiated-renegotiation-FIXED.md`. Running both arms is what
keeps those two from reading as damage from this change.

`cargo test -p cratonvm-native-io` — including two new unit tests for the fold
and, more importantly, for everything it must **not** touch (`::1`, `fe80::`,
`2001:db8::`, `::`, `::127.0.0.1`, plain v4).

**Deliberately not covered:** `org.apache.coyote.http2.TestHttp2Section_8_2`
and `TestLargeUpload` were dropped from the sweep. They exercise Tomcat's http2
**server**; the `http2.rs` change is in CratonVM's outbound h2 **client**
connect, which they never reach — and both are known throughput-wall classes
that cost >900 s per arm. Recorded here rather than silently truncated.

## Reproducing

```powershell
javac -d <dir> apps\tomcat-suite-runner\probes\Ipv6MappedProbe.java
<exe> -cp <dir> Ipv6MappedProbe          # CratonVM
java  -cp <dir> Ipv6MappedProbe          # HotSpot control

pwsh apps\tomcat-suite-runner\run-one.ps1 -Vm craton -Exe <exe> `
     -Class org.apache.catalina.startup.TestStartupIPv6Connectors
```
