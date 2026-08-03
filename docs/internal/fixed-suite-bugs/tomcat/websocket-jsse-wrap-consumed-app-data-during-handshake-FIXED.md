# WebSocket-over-TLS `[JSSE]` client connect: `wrap()` ate the caller's buffer during the handshake — FIXED

| | |
|---|---|
| **Status** | ✅ **FIXED** 2026-08-03 — retired from `docs/known-issues/tomcat/websocket-jsse-ssl-bytes-consumed-during-write.md` |
| **Defect** | `SSLEngine.wrap()` treated the caller's source buffer as application data for one flight of the handshake — consuming it, and ENCRYPTING it onto the wire |
| **Classes** | `TestWebSocketFrameClientSSL`, `TestWsWebSocketContainerSSL` — `[JSSE]` parameterization, both now `OK` |
| **Introduced by** | the doc-21 TLS-1.2 server-flight fix in `handshake_status_of` (the original doc's guess that this was fallout from that day's TLS work was right in spirit) |

## The original doc's stated root cause was wrong

It read:

> `"Bytes were consumed from the input during a write"` is `SSLEngine`'s own
> diagnostic for a wrap/unwrap contract violation

It is not an `SSLEngine` message at all. It is **Tomcat's own** string —
`asyncChannelWrapperSecure.check.wrap` in
`java/org/apache/tomcat/websocket/LocalStrings.properties` — thrown by
`AsyncChannelWrapperSecure.checkResult`:

```java
private void checkResult(SSLEngineResult result, boolean wrap) throws SSLException {
    ...
    if (wrap && result.bytesConsumed() != 0) {
        throw new SSLException(sm.getString("asyncChannelWrapperSecure.check.wrap"));
    }
```

So it is not a diagnostic about staged/unread bytes; it is Tomcat asserting the
JSSE contract that a **handshake-time `wrap()` consumes nothing from `src`**.
That reframing is what localises the bug: the engine's `bytesConsumed`, not its
record handling.

## Root cause

`do_wrap` (`native-builtins/src/t27_tls.rs`) decided "the caller is sending
application data" from `!conn.is_handshaking()`:

```rust
let needs_app_data = with_engine(id, |s| {
    s.conn.as_ref().map(|c| !c.is_handshaking()).unwrap_or(false)
}).unwrap_or(false);
```

But `is_handshaking()` and "the handshake is over as far as the CALLER knows"
are not the same instant. `handshake_status_of` deliberately keeps answering
`NEED_WRAP` after `is_handshaking()` flips false, until the engine's own final
flight has been drained — that is the TLS-1.2 server-flight fix recorded in
group 21, and it is load-bearing. A caller that correctly obeys that
`NEED_WRAP` calls `wrap(src, dst)` inside the window, and the engine had
already started treating `src` as payload.

Tomcat's WebSocket client hands that wrap a **static** 16921-byte buffer:

```java
private static final ByteBuffer DUMMY = ByteBuffer.allocate(16921);
...
SSLEngineResult r = sslEngine.wrap(DUMMY, socketWriteBuffer);
checkResult(r, true);
```

`CRATONVM_DBG=tls-hs` on the pre-fix binary, one `wss://` connect:

```
do_wrap id=1 RESULT status=OK hs=NEED_UNWRAP consumed=0     produced=244
do_wrap id=1 RESULT status=OK hs=FINISHED    consumed=16384 produced=16486
do_wrap id=3 RESULT status=OK hs=FINISHED    consumed=537   produced=639
```

Three things in that trace:

1. `consumed=16384` — the whole 16 KiB drain, which is exactly what
   `checkResult` refuses.
2. `produced=16486` — those 16384 zero bytes were **encrypted and written to
   the socket** mid-upgrade. This was never only a bogus counter.
3. Engine `id=3` is the *next* connection and finds only 537 bytes left.
   `DUMMY` is `static` and nobody rewinds it, so the position damage leaked
   into every later connection in the same JVM.

**Why only `[JSSE]`, and only WebSocket.** The `[OpenSSL-FFM]` parameterization
runs Tomcat's own `OpenSSLEngine`, which never reaches this native. And
Tomcat's server-side NIO handshake (`SecureNioChannel`) passes an *empty*
source buffer to its handshake wraps, so on the server path the same gate
consumed zero and nothing ever noticed.

**Which copy is live.** `javax/net/ssl/SSLEngine.wrap` is registered **twice** —
the rustls engine in `t27_tls.rs::register_sslengine_real`, and a synthetic
no-crypto stub in `tls.rs::register_ssl_engine`. The stub registers *later*, and
re-registration of a triple is last-write-wins, so it looks like the stub owns
the method. It does not: `register_synthetic_overrides` is
`#[cfg(feature = "synthetic-jdk")]` and never runs in the real-JDK mode the
suite uses. `t27_tls.rs` is the live copy; patching `tls.rs` would have changed
nothing.

## Fix

Gate app-data consumption on `handshake_finished_reported` — the flag the
engine already uses to mean "I have told the caller the handshake is over", and
the same flag `handshake_status_of` consults — so both halves of the engine
agree on when the handshake ends:

```rust
let needs_app_data = with_engine(id, |s| {
    s.handshake_finished_reported
        && s.conn.as_ref().map(|c| !c.is_handshaking()).unwrap_or(false)
}).unwrap_or(false);
```

and the same condition on `engine_wrap_pump`'s phase 1, so the invariant is
local to the helper and a future caller cannot reintroduce it.

No path can starve an application write: `handshake_status_of` only ever
returns `NOT_HANDSHAKING` once `handshake_finished_reported` is set, and that
flag is set exactly where `FINISHED` is handed back to the caller (`do_wrap`
and `do_unwrap`). So `FINISHED` is always delivered, and the wrap after it
consumes normally.

## Regression test

`t27_tls::tests::wrap_consumes_no_app_data_before_finished_is_reported` reuses
the existing `wp51_loopback_handshake_via_engine_state` fixture: drive a real
client↔server handshake through the pumps, stop in the exact window
(`!is_handshaking()` yet `!handshake_finished_reported`), and assert a
16921-byte source buffer is untouched — then assert it *is* consumed once
`FINISHED` has been reported, so the fix cannot wedge the post-handshake write
path.

**Proven non-vacuous:** with only the `engine_wrap_pump` gate reverted it fails
with `left: 16921, right: 0`; restored, it passes. `cargo test -p
cratonvm-native-builtins --lib -- t27_tls` → **38 passed, 0 failed** (includes
the pre-existing loopback / mTLS / SNI / session-resumption tests).

## Verification — same fixture, two binaries

`cratonvm-wsjsse-base-20260803.exe` (dev `8c4820eb0`, unmodified) vs
`cratonvm-wsjsse-fix-20260803.exe` (same tree + this fix), one process per
class, status from the JUnit banner. `bc` = occurrences of "Bytes were consumed
from the input during a write".

| Class | BASE (pre-fix) | FIX |
|---|---|---|
| `websocket.TestWebSocketFrameClientSSL` | **FAIL** bc=4 | **PASS** `OK (6 tests)` bc=0 |
| `websocket.TestWsWebSocketContainerSSL` | **FAIL** bc=2 | **PASS** `OK (3 tests)` bc=0 |
| `util.net.TestSsl` | FAIL bc=0 | FAIL bc=0 † |
| `util.net.TestSSLHostConfig` | PASS | PASS `OK (11)` |
| `util.net.TestSSLHostConfigCipher` | PASS | PASS `OK (12)` |
| `util.net.TestSSLHostConfigCompat` | PASS | PASS `OK (78)` |
| `util.net.TestSSLHostConfigIntegration` | PASS | PASS `OK (3)` |
| `util.net.TestSSLHostConfigProtocol` | PASS | PASS `OK (12)` |
| `util.net.TestClientCert` | FAIL bc=0 | FAIL bc=0 † |
| `util.net.TestClientCertTls13` | PASS | PASS `OK (6)` |
| `util.net.TestCustomSsl` | PASS | PASS `OK (1)` |
| `util.net.TestCustomSslTrustManager` | PASS | PASS `OK (9)` |
| `util.net.TestSslHandshakeFailure` | PASS | PASS `OK (1)` |
| `util.net.TestLargeClientHello` | PASS | PASS `OK (1)` |
| `util.net.TestPQC` | PASS | PASS `OK (39)` |
| `authenticator.TestSSLAuthenticator` | PASS | PASS `OK (1)` |

The two WebSocket rows are the **only** rows that moved. The `bc` count is 0
everywhere after the fix, including on the classes that still fail.

† Pre-existing on BOTH arms, identical failing method each time, and both are
the known **renegotiation** residual — rustls does not renegotiate:
`TestSsl.testClientInitiatedRenegotiation[JSSE]` and
`TestClientCert.testClientCertPostZero[JSSE]` (the latter is already recorded
as group 21's one by-design residual). Not touched by this change; tracked
separately.

## Note on the retired doc's repro block

It used the per-flag env spelling, which is legacy — the VM answers with:

```
[cratonvm] 4 per-flag variable(s) set directly; the supported spelling is now:
CRATONVM_THREADS=-default-watchdog CRATONVM_REAL=aqs CRATONVM_REAL=net-sockets CRATONVM_JIT=rootsnap-cache
```

Both arms above were run through the same harness with the same spelling, so
the comparison is apples-to-apples either way, but the grouped spelling is what
a fresh repro should use:

```powershell
$env:CRATONVM_REAL='net-sockets,aqs'; $env:CRATONVM_THREADS='-default-watchdog'
$env:CRATONVM_JIT='rootsnap-cache'; $env:CRATONVM_DBG='tls-hs'   # tls-hs optional
```
