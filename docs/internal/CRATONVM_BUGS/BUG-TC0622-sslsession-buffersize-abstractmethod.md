# Bug TC0622 — `SSLSession.getApplicationBufferSize()` / `getPacketBufferSize()` resolve to the abstract interface method (`AbstractMethodError: ... has no Code attribute`), killing the NioEndpoint socket processor

> **✅ RESOLUTION 2026-06-23 — FIXED. It WAS a missing registration, just in the
> WRONG location; the CORRECTION block below (which concluded "deeper VM dispatch
> fix") is SUPERSEDED.** Empirical tracing (probe under `CRATONVM_DBG_NOCODE=1`)
> confirmed that in real-JDK mode *no* `javax/net/ssl/SSLSession` native is
> registered at all — `getProtocol`/`getCipherSuite` throw the same
> `AbstractMethodError` as the buffer-size methods. Root cause:
>
> - The real-mode session object is allocated by `SSLEngineImpl.getSession()`
>   (`t27_tls.rs` ~3263, 7-field) and `SSLServerSocket.accept()` (~1014, 3-field).
>   `getSession` is registered by `t27_tls::register_sslengine_real`, which IS in
>   `register_essential_natives` (real-mode, ungated). ✅
> - But EVERY `javax/net/ssl/SSLSession` accessor native — `getProtocol`,
>   `getCipherSuite`, **and** `getApplicationBufferSize`/`getPacketBufferSize` —
>   lived ONLY in `phases_late::register_p68_ssl` and `tls::register_ssl_session`,
>   both reached only through `register_synthetic_overrides`, which is
>   `#[cfg(feature = "synthetic-jdk")]` and therefore COMPILED OUT of the real CLI.
> - The CORRECTION's prior attempt failed because it added the natives to
>   `register_p68_ssl` (compiled out in real mode) — so it changed nothing. The
>   CORRECTION misread "registered native still throws" as a dispatch bug; in fact
>   the native was never in the registry in real mode.
>
> **Fix (branch `fix/tomcat-sslsession-buffersize`, worktree `CratonVM-tcssl`):**
> new `register_ssl_session_real(r)` in `t27_tls.rs`, called from
> `register_sslengine_real` (the REAL-mode path). Registers `getApplicationBufferSize`
> (16384) / `getPacketBufferSize` (16709) as layout-independent constants, plus
> `getProtocol`/`getCipherSuite`/`isValid`/`getCreationTime`/`getLastAccessedTime`
> with field-count disambiguation between the 3-field and 7-field session shapes.
>
> **Verified:** the 3-line `SslBufProbe` now prints `proto=TLSv1.3`,
> `cipher=TLS_AES_256_GCM_SHA384`, `appBuf=16384`, `pktBuf=16709` (no
> `AbstractMethodError`). `TestSsl` now gets PAST the socket-processor death — the
> HTTPS connector initializes, the keystore/cert loads, and the service starts.
>
> **Residual (NOT this bug):** `TestSsl` still does not reach a green JUnit
> summary because it then hits the **known, OPEN, dominant Group-04 embedded-server
> throughput wall** (`docs/internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md`):
> a stack dump shows the main thread slowly grinding (pc advancing) through Xerces
> DTD parsing in `StandardServer.initInternal → MBeanUtils.<clinit> →
> Registry.loadDescriptors → Digester.parse` — i.e. `update_root_snapshot`
> interpreter throughput, explicitly documented as "NOT a TLS bug … the server
> actually starts/serves/tears-down correctly, just slowly." TestSsl's 21 methods
> × per-deploy cost exceed any practical per-class timeout. That wall is out of
> scope for this TLS fix.

> **⚠️ CORRECTION 2026-06-23 — deeper than a missing registration; NOT a quick
> win.** A fix attempt that simply registered `getApplicationBufferSize`/
> `getPacketBufferSize` natives in `register_p68_ssl` was built and **did not
> work** — the `AbstractMethodError` persisted. Direct probing
> (`SSLContext.getInstance("TLS").createSSLEngine().getSession()`) shows the
> object's class is literally `javax.net.ssl.SSLSession` (the **interface**),
> and **`getProtocol()` and `getCipherSuite()` — natives that
> `register_p68_ssl` already registers — ALSO throw `AbstractMethodError` on
> it.** So no `javax/net/ssl/SSLSession` native dispatches on the
> `SSLEngine.getSession()` object at all: the interpreter resolves the method to
> the abstract interface declaration (no Code) and throws before consulting the
> native registry. **Real root cause:** the SSLEngine session is allocated as
> the *interface* type `javax/net/ssl/SSLSession` (an `alloc_concurrent_synthetic`
> on the interface), on which method dispatch finds only abstract decls — OR
> native dispatch must consult the registry on a no-Code method. Registering more
> natives on the interface cannot help until one of those is addressed. **This is
> a deeper VM dispatch/allocation fix (handoff), not the bounded registration
> originally proposed below.** The registration attempt was reverted, not merged.

> **One-line root cause:** in real-JDK mode the `javax/net/ssl/SSLSession`
> objects handed to Tomcat are registered by `phases_late.rs::register_p68_ssl`
> (the "NEW-13" block) and `t27_tls.rs` (`SSLEngineImpl.getSession()`), and
> **neither block registers a native for `getApplicationBufferSize()I` or
> `getPacketBufferSize()I`**. The only registrant of those two methods is
> `tls.rs::register_ssl_session`, which is reachable solely from
> `register_synthetic_overrides` — a function gated `#[cfg(feature =
> "synthetic-jdk")]` and therefore **compiled out of the real-JDK CLI**. So an
> `invokeinterface SSLSession.getApplicationBufferSize` resolves to the abstract
> interface declaration (no Code, no native) and throws `AbstractMethodError`.

**Severity:** High (HIGH blast radius — ~18 TLS/HTTPS connector + WebSocket-SSL
+ HTTP/2-TLS classes; the selector loop dies so the HTTPS server never serves
and the affected tests HANG).

**Status on CratonVM:** HANG / FAIL (socket processor dies → server never
accepts the TLS handshake). **HotSpot:** PASS.

**Run date:** 2026-06-22
**Binary:** dev `df11ac00` (worktree `C:\craton\CratonVM-tctest`,
exe `C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe`).

## Affected classes (18)

From `…/results/tcfull0622/craton/` (grep `getApplicationBufferSize` /
`getPacketBufferSize` / `SSLSession` / `AbstractMethodError`):

- `org.apache.tomcat.util.net.TestSsl`
- `org.apache.tomcat.util.net.TestCustomSsl`
- `org.apache.tomcat.util.net.TestCustomSslTrustManager`
- `org.apache.tomcat.util.net.TestSslHandshakeFailure`
- `org.apache.tomcat.util.net.TestClientCert`
- `org.apache.tomcat.util.net.TestClientCertTls13`
- `org.apache.tomcat.util.net.TestAlpnFallback`
- `org.apache.tomcat.util.net.TestSSLHostConfigCipher`
- `org.apache.tomcat.util.net.TestSSLHostConfigCompat`
- `org.apache.tomcat.util.net.TestSSLHostConfigProtocol`
- `org.apache.tomcat.util.net.ocsp.TestOcspSoftFail`
- `org.apache.tomcat.security.TestSecurity2017Ocsp`
- `org.apache.coyote.http2.TestLargeUpload`
- `org.apache.coyote.http2.TestHttp2Section_8_3`
- `org.apache.tomcat.websocket.TestWsWebSocketContainerSSL`
- `org.apache.tomcat.websocket.TestWebSocketFrameClientSSL`
- `org.apache.catalina.valves.rewrite.TestResolverSSL`
- `org.apache.catalina.manager.TestManagerWebappSsl`

## Symptom

The Tomcat NioEndpoint socket processor throws on every accepted TLS
connection, so the selector loop dies and the HTTPS server never serves:

```
ERROR [org.apache.tomcat.util.net.NioEndpoint] Error running socket processor
  (java/lang/AbstractMethodError: method javax/net/ssl/SSLSession.getApplicationBufferSize()I has no Code attribute)
```

(The sibling `getPacketBufferSize()I` is thrown by the same code path; the
WebSocket-SSL classes surface `getPacketBufferSize` first.) Tomcat's
`SecureNio2Channel` / `SecureNioChannel` size their network and application
`ByteBuffer`s from `SSLEngine.getSession().getPacketBufferSize()` /
`.getApplicationBufferSize()` when the channel is created, so the very first
read after the handshake hits the missing method. With the processor dead the
client never completes the handshake and the test waits forever (HANG); on
HotSpot all of these PASS.

Direct minimal repro reproduces the exact error:

```java
SSLContext ctx = SSLContext.getInstance("TLS"); ctx.init(null, null, null);
SSLSession s = ctx.createSSLEngine().getSession();
s.getApplicationBufferSize();   // -> AbstractMethodError ("has no Code attribute")
```

`s.getClass().getName()` prints `javax.net.ssl.SSLSession` — confirming the
receiver's runtime class IS the interface name (CratonVM uses the interface
string as the synthetic concrete class), and that dispatch on it finds no
native.

## Root cause (pinned to the Rust site)

CratonVM hands out an `SSLSession` whose runtime class is the bare interface
`javax/net/ssl/SSLSession`. Two real-mode sites allocate it:

1. `native-builtins/src/t27_tls.rs` ~line 3232 —
   `SSLEngineImpl.getSession()` → `alloc_concurrent_synthetic(ctx,
   "javax/net/ssl/SSLSession", 7)` (the rustls-backed real engine path used
   under `CRATONVM_REAL_NET_SOCKETS=1`).
2. `native-builtins/src/phases_late.rs::register_p68_ssl` ~line 31902 (the
   "NEW-13" block) registers the `javax/net/ssl/SSLSession` accessors:
   `getProtocol`, `getCipherSuite`, `isValid`, `getId`, `getPeerCertificates`,
   `getCreationTime`, `getLastAccessedTime` — **but not**
   `getApplicationBufferSize` or `getPacketBufferSize`.

The native dispatch (`vm/src/vm/vm_exec.rs` ~line 10914, and the equivalent in
`interpreter.rs` ~line 2713) handles an abstract resolved method by looking up
`native_methods.find("javax/net/ssl/SSLSession", "getApplicationBufferSize",
"()I")`. That lookup returns `None`, so the interpreter falls through to the
abstract interface declaration (no Code attribute) and throws
`AbstractMethodError` (`interpreter.rs` ~line 3018).

The **only** place these two methods are ever registered is
`native-builtins/src/tls.rs::register_ssl_session` (lines 668–676):

```rust
let cls = "javax/net/ssl/SSLSession";
r.register(cls, "getApplicationBufferSize", "()I", |_,_| Ok(Some(Value::Int(16384))));
r.register(cls, "getPacketBufferSize",      "()I", |_,_| Ok(Some(Value::Int(16709))));
```

…but `register_ssl_session` is called only from `register_tls_natives`, which
is called only from `register_synthetic_overrides` (`lib.rs` line 10000):

```rust
#[cfg(feature = "synthetic-jdk")]
pub fn register_synthetic_overrides(registry: &mut NativeMethodRegistry) { … register_tls_natives(registry); … }
```

The Tomcat full-suite binary runs in **real-JDK mode** (no `synthetic-jdk`
feature), so `register_synthetic_overrides` — and with it the entire
`tls.rs::register_ssl_session` block — is **compiled out**. The buffer-size
natives therefore never reach the registry in this build, even though
`tls.rs`'s own unit test (`assert!(r.find(cls,"getApplicationBufferSize","()I").is_some())`)
passes in isolation (it builds its own registry and calls `register_tls_natives`
directly). The registry itself is last-writer-wins per `(class,method,desc)` and
applies no SSL-specific drop, so this is purely a *missing* registration in the
real-mode path, not an override being lost.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$exe = "C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe"
$cp  = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
& $exe -Xmx2g -cp $cp org.junit.runner.JUnitCore org.apache.tomcat.util.net.TestSsl
# -> repeated "Error running socket processor (... getApplicationBufferSize()I has no Code attribute)", then hangs.
```

Minimal (no Tomcat) repro — the 3-line `SslBufProbe` above throws the same
`AbstractMethodError` immediately.

## Recommendation — **FIX** (bounded missing-native)

Register the two methods in the **real-mode** SSLSession block, i.e. add to
`native-builtins/src/phases_late.rs::register_p68_ssl` alongside the existing
`getProtocol`/`getCipherSuite`/`isValid` accessors (~line 31902):

```rust
r.register(ssl_session, "getApplicationBufferSize", "()I",
    |_ctx, _args| Ok(Some(Value::Int(16384))));   // TLS max plaintext record
r.register(ssl_session, "getPacketBufferSize", "()I",
    |_ctx, _args| Ok(Some(Value::Int(16709))));   // 16384 + TLS record overhead (5+256+68)
```

These are the standard JSSE constants (`SSLSession.getPacketBufferSize()`
returns 16709, `getApplicationBufferSize()` returns 16384 for TLS in the real
JDK) and match the values already used by the (compiled-out) `tls.rs`
implementation — Tomcat only needs them to be ≥ a TLS record so its
`ByteBuffer`s are large enough. Purely additive: the methods previously always
threw. This single registration covers every affected class (all of them go
through `NioEndpoint`/`Secure*Channel` buffer sizing). No VM/test source was
modified as part of this investigation.
