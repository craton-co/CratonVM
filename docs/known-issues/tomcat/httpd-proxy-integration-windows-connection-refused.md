# Fixture gap: httpd reverse-proxy integration tests can't reach httpd on the Windows fixture — 7 classes

| | |
|---|---|
| **Status** | Fixture gap, NOT a CratonVM bug |
| **HotSpot** | Fails identically (connection-refused / connect failure to the proxy) |
| **Discovered** | 2026-08-03, rerunning the 07-31 4-shard FAIL/HANG set after merging `dev` (`c1fe51a24`) |

## Symptom

7 `org.apache.tomcat.integration.httpd.*` classes fail with a client-side
`ConnectException` to a loopback port, i.e. nothing is listening where the
test expects the Apache httpd reverse proxy to be:

```
1) testBasicProxying(org.apache.tomcat.integration.httpd.TestBasicProxy)
java.net.ConnectException: connect [::1]:54501: ... (os error 10061)
```

Same pattern (different ports) for: `TestFullReverseProxy`,
`TestLargePayloadWithProxy`, `TestRemoteIpValveWithProxy`,
`TestSessionWithProxy`, `TestSSLValveWithProxy01`, `TestSSLValveWithProxy02`.

Verified on the fresh HotSpot baseline: `TestChunkedTransferEncodingWithProxy`
(the 8th class in this family) also fails on HotSpot, but with a different
symptom — an `HttpURLConnection.connect()` failure rather than a clean
`ConnectException` — while CratonVM's failure mode for that one specific
class is an `OutOfMemoryError: Java heap space (native primitive array of
length 1048576000)` inside `testChunkedTransferEncoding`. Both VMs fail this
class; the differing failure *shape* (OOM vs. connect exception) is worth a
follow-up if httpd integration is ever brought up on Windows, but the root
cause — httpd not running/reachable — is shared, so it's grouped here rather
than filed as a separate CratonVM defect.

## Not a CratonVM bug

This project's only existing httpd-integration doc,
`docs/internal/tomcat/missing-httpd-binary.md`, covers the **Linux/Azure**
fixture's missing httpd binary (from the 2026-07-28/29 fixture-completion
work). This is the **Windows** analog: no Apache httpd process is
started/configured as part of `apps/tomcat`'s Windows suite setup, so every
test in this family that expects to proxy through httpd fails identically on
both VMs — a fixture completion gap, not VM behavior.

## Fix

Install and configure Apache httpd (with `mod_proxy`/`mod_jk` or
`mod_proxy_ajp` as the specific tests require) on the Windows suite host, and
wire its start/stop into the suite runner the same way the Linux fixture does
(if it does). Once fixed, re-run all 8 classes — especially
`TestChunkedTransferEncodingWithProxy` — to confirm no CratonVM-side defect
remains once httpd is actually reachable.
