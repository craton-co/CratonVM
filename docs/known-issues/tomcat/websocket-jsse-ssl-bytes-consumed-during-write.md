# WebSocket-over-TLS client handshake fails under JSSE: `SSLException: Bytes were consumed from the input during a write`

| | |
|---|---|
| **Status** | OPEN |
| **Severity** | medium — breaks the `[JSSE]` parameterization of WebSocket-SSL client connects; `[OpenSSL-FFM]` parameterization of the same classes passes |
| **HotSpot** | PASS, both classes fully green (fresh-verified 2026-08-03) |
| **CratonVM** | FAIL, `[JSSE]` param only |
| **Discovered** | 2026-08-03, rerunning the 07-31 4-shard FAIL/HANG set after merging `dev` (`c1fe51a24`) |

## Symptom

Two classes, same signature, only under the `[JSSE]` SSL implementation
parameterization (their `[OpenSSL-FFM]` counterparts pass):

```
1) testBug56032[JSSE](org.apache.tomcat.websocket.TestWebSocketFrameClientSSL)
jakarta.websocket.DeploymentException: The HTTP request to initiate the
WebSocket connection to [wss://localhost:58343/firehose] failed
	at org.apache.tomcat.websocket.WsWebSocketContainer.connectToServerRecursive(WsWebSocketContainer.java:458)
Caused by: java.util.concurrent.ExecutionException:
javax.net.ssl.SSLException: Bytes were consumed from the input during a write
```

```
1) testConnectToServerEndpointSSL[JSSE](org.apache.tomcat.websocket.TestWsWebSocketContainerSSL)
jakarta.websocket.DeploymentException: The HTTP request to initiate the
WebSocket connection to [wss://localhost:65280/echoAsync] failed
Caused by: java.util.concurrent.ExecutionException:
javax.net.ssl.SSLException: Bytes were consumed from the input during a write
```

`"Bytes were consumed from the input during a write"` is `SSLEngine`'s own
diagnostic for a wrap/unwrap contract violation — the caller invoked
`wrap()` (or a related write-path operation) while the engine still had
unread bytes staged from an `unread()`/`unwrap()` call it expected to be
consumed first. This is specific to the JSSE `SSLEngine` backend; the
OpenSSL-FFM backend's engine doesn't hit it, which points at the JSSE-side
`SSLEngine` driving code in the WebSocket upgrade path, not the TLS records
themselves.

Given the large volume of TLS/`SSLEngine`/hostname-verification work that
landed in `dev` today (`fec4d208e fix(tls): a HostnameVerifier is JSSE's
FALLBACK, not an extra gate`, plus the `TestSsl`/`TestSSLHostConfig*` fix
series), this may be a narrow regression from that work rather than a
long-standing gap — not yet bisected.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1; $env:CRATONVM_ROOTSNAP_CACHE=1
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g -cp (Get-Content .suite\cp.txt) `
  org.junit.runner.JUnitCore org.apache.tomcat.websocket.TestWebSocketFrameClientSSL
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g -cp (Get-Content .suite\cp.txt) `
  org.junit.runner.JUnitCore org.apache.tomcat.websocket.TestWsWebSocketContainerSSL
```

HotSpot control (both classes): fully green (`OK (6 tests)` / `OK (3 tests)`).

## Suspected root cause (not yet isolated)

Not yet checked against source. Candidates: the WebSocket upgrade path reuses
a buffer/engine state object across the HTTP-Upgrade `SSLEngine` unwrap and a
subsequent wrap without resetting a "pending unread bytes" flag, or a
double-consumption of the TLS record buffer during the upgrade handshake
specifically (this failure mode is orthogonal to a normal HTTPS request,
since plain `TestSsl`/`TestSSLHostConfig*` classes are green — it's
specific to the WebSocket upgrade's mixed HTTP+TLS handshake sequencing). No
prior known-issue doc covers this signature (checked `docs/internal/fixed-suite-bugs`
and `docs/known-issues` — no hits).
