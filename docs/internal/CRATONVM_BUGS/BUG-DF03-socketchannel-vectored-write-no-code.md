# Bug DF03 — `SocketChannel.write(ByteBuffer[], int, int)` has no Code attribute → AbstractMethodError

> **✅ FIXED 2026-06-17** (worktree `C:/craton/CratonVM-dfnet`, branch
> `fix/tomcat-df03-df04-nio`, `native-io/src/socket_channel.rs`). Registered the
> gathering `write([Ljava/nio/ByteBuffer;II)J` and scattering
> `read([Ljava/nio/ByteBuffer;II)J` (plus the 1-arg convenience forms) on both
> `java/nio/channels/SocketChannel` and `sun/nio/ch/SocketChannelImpl`, looping
> over the buffer slice and reusing the existing `TcpStream` + ByteBuffer
> plumbing (concatenate-then-write for gather; read-once-then-scatter for
> scatter; per-buffer position advance; EAGAIN→0, EOF→-1). **Verification:**
> standalone repro `scratch/dfnet/VectorIORepro.java` (blocking SocketChannel
> pair, no selector → DF01-independent) matches HotSpot exactly: `written=22`,
> `posA=7 posB=15`, `read=22`, content `"Hello, vectored world!"`,
> `DF03_RESULT=PASS`. No more AbstractMethodError.

**Severity:** Medium (bounded missing-method gap; breaks websocket serving).
**Status on CratonVM:** serving error → HANG. **HotSpot:** PASS.
**Run date:** 2026-06-17
**Binary:** dev `77620f55` (worktree `C:/craton/CratonVM-tcfull`).
**Affected classes (2 directly; more websocket tests downstream):**
`org.apache.tomcat.websocket.server.TestClose`,
`org.apache.tomcat.websocket.server.TestKeyHeader`.

## Symptom

When the connector flushes a response with a *vectored* (gathering) write, the
VM throws `AbstractMethodError` because the gathering-write overload has no body:

```
ERROR [org.apache.coyote.http11.Http11NioProtocol] Error reading request, ignored
   (java/lang/AbstractMethodError: method
    java/nio/channels/SocketChannel.write([Ljava/nio/ByteBuffer;II)J has no Code attribute)
INFO  [...TestClose] onClose: CloseReason: code [1006],
    reason [method java/nio/channels/SocketChannel.write([Ljava/nio/ByteBuffer;II)J has no Code attribute]
```

The scalar `write(ByteBuffer)` works, but the three-arg gathering form
`write(ByteBuffer[] srcs, int offset, int length)` (declared abstract on
`java.nio.channels.GatheringByteChannel` / `SocketChannel`) resolves to a method
with **no Code attribute** and no registered native → `AbstractMethodError`.

## Root cause (analysis)

CratonVM's `SocketChannel` implementation (the concrete `sun.nio.ch.SocketChannelImpl`
or the CratonVM socket-channel native layer in `native-io/src/socket_channel.rs`)
provides the single-buffer `write`/`read` but not the vectored
`write(ByteBuffer[],int,int)` / `read(ByteBuffer[],int,int)` overloads that
`GatheringByteChannel`/`ScatteringByteChannel` declare. Tomcat's NIO write path
(`NioChannel`/`SocketWrapperBase`) uses the gathering form for header+body
flushes, so it hits the unimplemented method.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$CP = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS="1"; $env:CRATONVM_REAL_AQS="1"; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG="1"
C:\craton\CratonVM-tcfull\target\release\cratonvm.exe -Xmx2g -cp $CP `
  org.junit.runner.JUnitCore org.apache.tomcat.websocket.server.TestClose
```

## Recommendation

**FIX (bounded).** Implement the gathering `write(ByteBuffer[],int,int)` and the
scattering `read(ByteBuffer[],int,int)` on the socket channel (loop over the
buffer slice delegating to the existing scalar native, or a single vectored
native). Tractable and self-contained.
