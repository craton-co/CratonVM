# `TestWebSocketFrameClient(SSL)#testConnectToServerEndpoint` — large WebSocket message truncated mid-read, GC-independent

| | |
|---|---|
| **Status** | OPEN, newly found 2026-08-13. Not a GC-specific bug — reproduces under Generational, G1, and ZGC alike, with a different truncation point each run. |
| **Symptom** | `AssertionError: expected:<100000> but was:<N>` where `N` varies run to run and is always well short of 100000 — the client never receives the full message. |
| **Discovered** | 2026-08-12/13, complete 651-class Tomcat suite run under all 3 GC backends. |

## Both the plain and SSL variant, same failure, same test method

Both classes fail on exactly `testConnectToServerEndpoint` (SSL variant: `testConnectToServerEndpoint[JSSE]`) and nowhere else — the rest of each class passes (4/4 and 6/6 respectively, minus this one). The received byte count is different every time, on every backend:

| Class | Generational | G1 | ZGC |
|---|---:|---:|---:|
| `TestWebSocketFrameClient` | 48105 | 23875 | 56139 |
| `TestWebSocketFrameClientSSL` | 50766 | 21389 | 54121 |

Non-deterministic, partial-but-nonzero, and consistently well under half the expected 100000 bytes — the shape of a connection or read loop terminating early under a race, not a fixed off-by-N or a total failure to connect.

## Not the already-known permanent gaps in this area

Two other TLS-adjacent classes in the same suite run also failed, but their failures are **documented, permanent, by-design gaps**, unrelated to this one — noted here so nobody conflates them:

- `TestSsl#testClientInitiatedRenegotiation[JSSE]` and `TestClientCert#testClientCertPostZero[JSSE]` both fail because rustls does not implement TLS 1.2 renegotiation (a deliberate CVE-2009-3555/3SHAKE mitigation) — see `testssl-client-initiated-renegotiation-FIXED.md` and `21-tls-handshake-enforcement-gap-FIXED.md`, both of which explicitly carve out exactly these two tests as permanent residuals, re-confirmed as of 2026-08-02/03 at exactly "1 failure" each. This run's `TestSsl`/`TestClientCert` results match those exact counts and exact failing methods — not a regression, not related to this doc.

This truncation, by contrast, is not covered by any existing internal doc. `websocket-jsse-wrap-consumed-app-data-during-handshake-FIXED.md` fixed a real defect in this same area (an `SSLEngine.wrap()` handshake-window bug) and its own verification table shows `TestWebSocketFrameClientSSL` going from failing to a clean `OK (6 tests)`, zero failures — so this is either a regression against that fix, or a distinct bug in the same code path that fix's narrower test matrix didn't happen to trigger.

## Not chased further here

Root cause not investigated — this doc only establishes that the symptom is real, reproducible, GC-independent, and not one of the area's known permanent gaps. Worth checking first: whether the message is sent as multiple WebSocket frames (a multi-frame reassembly bug would produce exactly this shape — a partial prefix received before something drops the rest) and whether the truncation point correlates with any fixed buffer size in the native WebSocket read path.

## Reproduction

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
apps\tomcat-suite-runner\run-one.ps1 -Class org.apache.tomcat.websocket.TestWebSocketFrameClient -Exe <cratonvm.exe>
apps\tomcat-suite-runner\run-one.ps1 -Class org.apache.tomcat.websocket.TestWebSocketFrameClientSSL -Exe <cratonvm.exe>
```

Given the non-deterministic truncation point, expect it to reproduce on most but not necessarily every run.
