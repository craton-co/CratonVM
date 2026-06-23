# Bug DF04 — `sun.nio.ch.Net.available(FileDescriptor)I` missing native → UnsatisfiedLinkError

> **✅ FIXED 2026-06-17** (worktree `C:/craton/CratonVM-dfnet`, branch
> `fix/tomcat-df03-df04-nio`, `native-io/src/net.rs`). Registered
> `sun/nio/ch/Net.available(Ljava/io/FileDescriptor;)I`, returning the readable
> byte count via `ioctlsocket(FIONREAD)` (Windows, raw `#[link(name="Ws2_32")]`
> FFI — same no-extra-crate pattern as the WSAPoll selector) / `libc::ioctl`
> (Unix), looked up in the `net_sockets` registry (NioSocketImpl path), with a
> `socket_channel::tcp_stream_available` fallback. Unknown/closed/non-stream fd →
> 0 (a legal `available()` answer) rather than throwing. **Verification:**
> standalone repro `scratch/dfnet/NetAvailRepro.java` (blocking Socket pair, no
> selector → DF01-independent) matches HotSpot: `available=20` (20 bytes sent),
> bytes intact, `DF04_RESULT=PASS`. No more UnsatisfiedLinkError.

**Severity:** Medium-High (missing native; 19 server-test classes touch it).
**Status on CratonVM:** FAIL / HANG. **HotSpot:** PASS.
**Run date:** 2026-06-17
**Binary:** dev `77620f55` (worktree `C:/craton/CratonVM-tcfull`).
**Affected classes (19):** incl.
`jakarta.servlet.TestServletRequestParametersFormUrlEncoded`,
`jakarta.servlet.TestServletRequestParametersMultipartEncoded`,
`org.apache.catalina.authenticator.TestFormAuthenticatorA/B/C`,
`org.apache.catalina.connector.TestClientReadTimeout`,
`org.apache.catalina.connector.TestRequest`,
`org.apache.catalina.connector.TestCoyoteAdapterCanonicalization`,
`org.apache.catalina.connector.TestCoyoteAdapterRequestFuzzing`,
`org.apache.catalina.core.TestStandardContext`,
`org.apache.catalina.servlets.TestDefaultServlet`,
`org.apache.catalina.servlets.TestWebdavServlet`,
`org.apache.coyote.http11.TestHttp11InputBuffer(/CRLF)`,
`org.apache.tomcat.util.http.TestCookieParsing`,
`org.apache.tomcat.util.http.TestMimeHeadersIntegration`,
`org.apache.catalina.manager.TestHostManagerWebapp`,
`org.apache.catalina.manager.TestStatusTransformer`.

## Symptom

The connector's read path queries the number of readable bytes on the socket and
the native is not registered:

```
java.lang.UnsatisfiedLinkError: sun/nio/ch/Net.available(Ljava/io/FileDescriptor;)I
```

## Root cause (analysis)

`sun.nio.ch.Net.available(FileDescriptor)` (the `ioctl FIONREAD` equivalent used
to report how many bytes can be read without blocking) has no CratonVM native
binding. The NIO connector calls it while processing a request, so the request
fails (or the server stalls and the test times out).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$CP = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS="1"; $env:CRATONVM_REAL_AQS="1"; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG="1"
C:\craton\CratonVM-tcfull\target\release\cratonvm.exe -Xmx2g -cp $CP `
  org.junit.runner.JUnitCore jakarta.servlet.TestServletRequestParametersFormUrlEncoded
```

## Recommendation

**FIX (bounded, high count).** Register a `sun/nio/ch/Net.available(FileDescriptor)I`
native that returns the readable byte count for the socket fd (Windows
`ioctlsocket(FIONREAD)`), alongside the existing socket natives in `native-io`.
Several of these 19 classes also need DF01 (selector loop) to fully pass, but
`Net.available` is an independent, self-contained gap.
