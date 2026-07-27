# `TestLargeUpload` — HTTP/2 large POST body truncated (65535 expected, 13107 received)

**Status:** ✅ **FIXED** (2026-07-27, branch `fix/tomcat-http2-largeupload-20260727`,
commit `89787de3e`). `org.apache.coyote.http2.TestLargeUpload` = `OK (6 tests)`
on CratonVM, matching HotSpot.

## Symptom

```
1) testLargePostRequest[0: true JSSE]](org.apache.coyote.http2.TestLargeUpload)
java.lang.AssertionError: expected:<65535> but was:<13107>
```

The test POSTs five HTTP/2 `DATA` frames of 13107 bytes each (65535 total,
end-of-stream carried by a trailer `HEADERS` frame) over TLS and asserts the
servlet read all of them. Only the first frame's worth arrived — 13107 bytes,
exactly 1/5 of the total.

## The 1/5 ratio was a red herring

The original triage guessed HTTP/2 flow control, because 65535 is the default
`INITIAL_WINDOW_SIZE` and 13107 ≈ 65535/5. It was neither: 13107 is simply the
test's `bodySize`, i.e. **exactly one `DATA` frame**. Nothing in
`Http2UpgradeHandler`/`Stream`/`Http2Parser` was at fault. The bug was one
layer below, in CratonVM's native `SSLEngine`.

The decisive clue was in the parameter matrix, not the ratio: only
`[0: true JSSE]` failed. Parameter 1 is `Http2TestBase.useAsyncIO` — the
`false` (single-buffer) variant always passed.

## Root cause

`native-builtins/src/t27_tls.rs`, `do_unwrap`.

`SSLEngine.unwrap(src, dsts, offset, length)` is a **scattering** operation: it
fills the destination buffers in order, skipping ones that are already full.
Both of `do_unwrap`'s scatter loops (Step 0, the `plaintext_pending` drain; and
Step 3, the plaintext delivery) instead did:

```rust
let n = bb_write_from(ctx, *dst, &plaintext[idx..]);
produced_total += n;
idx += n;
if n == 0 {
    break;          // <-- treats a FULL buffer as a stop signal
}
```

A destination that accepts 0 bytes is **full**, not a terminator.

Tomcat's `Http2AsyncParser` (the `useAsyncIO=true` connector path, via
`SocketWrapperBase`'s vectored read and `SecureNioChannel.read(ByteBuffer[],
int, int)`) reads every frame into a fixed pair of buffers:

```java
ByteBuffer header       = ByteBuffer.allocate(9);
ByteBuffer framePayload = ByteBuffer.allocate(input.getMaxFrameSize());  // 16384
```

The first unwrap fills the 9-byte header. From then on **every** unwrap sees
`dsts[0]` full, hits `n == 0`, and breaks out before ever reaching
`framePayload` — which still had ~13 KiB of room.

By that point the TLS record had already been consumed and decrypted, so all
16384 plaintext bytes were stashed in the engine-private `plaintext_pending`
buffer. Tomcat cannot see that buffer. `SecureNioChannel.read` therefore saw
`BUFFER_OVERFLOW` with `read == 0`, appended its own read buffer as an extra
destination and retried — but the retry took the pending-drain path, which had
the *same* `n == 0` break, so it also produced 0. The connection wedged in a
`BUFFER_OVERFLOW` loop and was torn down (`closeOutbound()`), and the servlet's
`InputStream` ended after the single frame it had already received.

`useAsyncIO=false` passed because it goes through single-destination
`unwrap(src, dst)`, where there is never a preceding full buffer.

`do_wrap`'s gather loop over `srcs` does not have this bug — it only breaks on
a byte-count limit, never on a zero-length source.

### Trace (`CRATONVM_DBG_TLS_HS=1`)

Before:

```
do_unwrap id=2 RETURN(normal) status=OK              consumed=16406 produced=16384   <- header empty, works
do_unwrap id=2 RETURN(normal) status=BUFFER_OVERFLOW consumed=16406 produced=0       <- header full -> break
do_unwrap id=2 RETURN(pending-drain) status=BUFFER_OVERFLOW consumed=0 produced=0
JAVA_CALLED closeOutbound() id=2
```

After:

```
do_unwrap id=2 RETURN(normal) status=OK              consumed=16406 produced=16384
do_unwrap id=2 RETURN(normal) status=BUFFER_OVERFLOW consumed=16406 produced=13125
do_unwrap id=2 RETURN(pending-drain) status=OK       consumed=0     produced=3259
do_unwrap id=2 RETURN(normal) status=BUFFER_OVERFLOW consumed=16406 produced=9857
do_unwrap id=2 RETURN(pending-drain) status=OK       consumed=0     produced=6527
do_unwrap id=2 RETURN(normal) status=BUFFER_OVERFLOW consumed=16406 produced=6589
do_unwrap id=2 RETURN(pending-drain) status=OK       consumed=0     produced=9795
```

Each record now splits correctly between the caller's frame payload and (for
the remainder) Tomcat's own overflow read buffer, which is exactly the
`OverflowState.PROCESSING` path `SecureNioChannel.read` is written for.

## Fix

Drop the `if n == 0 { break; }` from both scatter loops so a full destination is
skipped rather than ending the scatter. Two hunks, no behavioural change for
single-destination `unwrap`.

## Verification

Host: Windows box, worktree `C:\craton\CratonVM-http2-largeupload-20260727`,
binary `cratonvm-h2lu-20260727.exe`, real-JDK mode, JIT on.

```powershell
# single class
.\apps\tomcat\.suite\run-h2lu.ps1 -Exe <cratonvm-h2lu-20260727.exe> -Out h2lu-fix1
```

| | before | after | HotSpot |
|---|---|---|---|
| `TestLargeUpload` | FAIL `expected:<65535> but was:<13107>` | **`OK (6 tests)`** | `OK (6 tests)` |

Regression sweep over all 73 `org.apache.coyote.http2.*` +
`org.apache.tomcat.util.net.*` classes, CratonVM vs a same-fixture HotSpot
baseline (`apps/tomcat/.suite/clsrun/{h2tls-craton-fix,h2tls-hotspot}`):

* **HTTP/2 group: 42/44 PASS.** The two exceptions are not this bug:
  * `TestHttp2Limits` — HANG at the 300s suite timeout under 3-way parallel
    load; **PASSes in 393s when run serially**. Throughput wall (group 04).
  * `TestHttp2Section_8_2` — HANGs on HotSpot too (doc 29).
* No regressions. Every remaining CratonVM-only non-PASS in the sweep belongs to
  an already-open doc or is a fixture gap:
  * `TestSsl`, `TestClientCert`, `TestCustomSslTrustManager`,
    `TestSSLHostConfig{Cipher,Compat,Protocol}`, `TestSslHandshakeFailure` —
    doc [21](21-tls-handshake-enforcement-gap.md), signature
    `Expected exception: javax.net.ssl.SSLHandshakeException` / client-cert
    rejection. `TestSsl`'s hang was **verified pre-existing**: a control binary
    built without this fix hangs at the identical point,
    `testSimpleSsl[OpenSSL]` (the tomcat-native/APR stub path).
  * `TestXxxEndpoint` — doc [27](27-xxxendpoint-unix-domain-socket-init-failure.md),
    `LifecycleException: Protocol handler initialization failed`.
  * `TestLargeClientHello` — `NoClassDefFoundError: org/bouncycastle/asn1/…`;
    BouncyCastle missing from the fixture classpath, fails on both VMs.
  * `TestPQC`, `TestOcspEnabled` — FAIL on HotSpot too.
  * `TestOcspSoftFailInternalError`, `TestOcspSoftFailTryLater` — CratonVM
    PASSes where HotSpot FAILs.

## Blast radius beyond Tomcat

Any caller of the array form of `SSLEngine.unwrap` that passes a
partially-filled leading buffer hit this. That is the standard shape for
protocol parsers that read a fixed-size header plus a payload in one vectored
call, so the fix is expected to help other TLS-over-vectored-read paths (e.g.
Netty/Jetty-style header+body reads), not just Tomcat's HTTP/2.
