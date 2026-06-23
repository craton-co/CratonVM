# Bug DF07 — WebSocket client connect → "connect failed … (os error 10049)" (address invalid)

> **✅ FIXED 2026-06-17 (root cause)** — merged to dev (`b6915a15`; fix commit
> `5199f4be`, branch `fix/df07-async-channel-transport`). The real transport
> blocker (below) was a **core small-socket-write bug**: `unsafe_offset()`
> (native-builtins/src/lib.rs) clamped any offset `> 1<<30` to `0` (a defense
> against compact-value drift). But the `base == null` form of
> `Unsafe.get*/put*(Object,long,..)` passes a full off-heap ADDRESS there, not a
> field index — and JDK-25 `DirectByteBuffer.put(byte)/get(byte)` (used for
> transfers of ≤6 elements, via `ScopedMemoryAccess` → `Unsafe.putByte(null,
> address+i, v)`) had its destination — a CratonVM Unsafe-arena handle (ARENA_TAG
> bit 62, always `> 1<<30`) — clamped to `0`. So the JDK-25 `NioSocketImpl` temp
> DirectByteBuffer wrote bytes to address 0 (lost) while `Net.write0` read the
> arena handle (zeros) → **every real `Socket.getOutputStream().write(byte[])` of
> ≤6 bytes silently delivered ZEROS** (≥7 bytes used bulk `copyMemory`, routed to
> the arena, worked). **Fix:** `unsafe_offset` passes a live arena handle through
> verbatim (a field index is never a live arena handle, so object-base access is
> unaffected) — fixes both small write (putByte) and read (getByte). Plus the
> synthetic `AsynchronousSocketChannel` Future-form transport now completes (real
> `CompletableFuture.completedFuture` + real `HeapByteBuffer` access in
> phases_late.rs). **Verified vs HotSpot** (DF01-independent repros in
> `scratch/dfnet/`): `Thresh` (1..32 bytes all PASS, was ≤4 FAIL), `MinWrite`,
> `SocketWriteRead`, `CharNet2`, `FutureFlowRepro` (websocket connect→write→read
> round-trip = `"PONG"`), plus DF03/DF04 + DirectByteBuffer/Unsafe/CHM/Atomic
> regressions — all PASS. (The earlier address-decode "family" fix was necessary
> but not sufficient; this small-write fix is what makes the transport work.)

> **⚠ REAL TRANSPORT BLOCKER FOUND 2026-06-17 (deep dive)** — branch
> `fix/df07-async-channel-transport` (worktree `C:/craton/CratonVM-dfnet`,
> repros in `scratch/dfnet/`). The async-channel "Future/completion + ByteBuffer
> model" is NOT the real blocker. The actual blocker is a **core small-socket-write
> bug**: a `Socket.getOutputStream().write(byte[])` (real `NioSocketImpl` path,
> `CRATONVM_REAL_NET_SOCKETS=1`) of **≤ ~6 bytes silently delivers ZEROS** to the
> peer (writes of ≥7 bytes work). Confirmed with `CRATONVM_SOCKET_CAPTURE`:
> `net_write0` itself receives zero bytes (`w fd=... len=4 00000000`).
>
> Root-cause chain (all bisected with `scratch/dfnet/` repros + `CRATONVM_DBG_NET`
> / `CRATONVM_DBG_ARENA` instrumentation now on the branch):
>  - It is **size-dependent, not direction-dependent** (`MinWrite.java`/`Thresh.java`:
>    len ≤ 4 FAIL, len ≥ 7 PASS, both client→server and server→client).
>  - It is **NOT generic DirectByteBuffer/Unsafe**: `ByteBuffer.allocateDirect(n)
>    .put(byte[]).get(byte[])` round-trips at all small sizes (`SmallCap.java`),
>    and `allocateDirect` uses **real memory** (no arena, no `newDirectByteBuffer`).
>  - The **socket** write path's temp buffer is built differently: JDK-25's
>    `Util.getTemporaryDirectBuffer` (used by `NioSocketImpl`/`IOUtil` for a heap
>    source) routes through FFM — `Arena.allocate` → `MemorySegment.asByteBuffer`
>    → `JavaNioAccess.newDirectByteBuffer(segAddr, size, segment)` — so the temp
>    `DirectByteBuffer.address` is a **CratonVM Unsafe-arena handle** (trace:
>    `newDirectByteBuffer addr=0x4000001000000000 cap=4`).
>  - Arena instrumentation proves the mismatch: for the failing small write, the
>    JDK's `bb.put(src)` does **NOT** copy the bytes into the arena (no `copy_in`
>    fires for the payload), yet `net_write0` does `copy_out` from that arena
>    handle and reads zeros. So the put writes to one location while `write0`
>    reads the segment-backed arena handle — a **CratonVM FFM `MemorySegment` ↔
>    `DirectByteBuffer` temp-buffer address inconsistency** in the small-write
>    path. (Larger writes land in the arena correctly and pass.)
>  - Independently, the async fd_table read path has a read-after-write anomaly
>    (returns the client's own write) — secondary, also same socket-IO layer.
>
> **Impact:** breaks ALL small real-`Socket` writes (websocket control frames,
> small HTTP writes, etc.), not just websockets. **This is the real DF07 fix
> target** — a focused core-VM FFM/NIO temp-buffer investigation, NOT the
> async-channel model. The branch carries the (correct but insufficient) async
> `completedFuture` + real-ByteBuffer fixes plus the diagnostic instrumentation;
> **nothing is merged to dev** (dev stays clean at the DF03/DF04/DF07-decode
> merge `e7802f23`). See [[reference_async_socket_channel_dual_impl]].

> **◑ PARTIALLY FIXED 2026-06-17** (worktree `C:/craton/CratonVM-dfnet`, branch
> `fix/tomcat-df03-df04-nio`). The stated root cause — the holder-blind
> `InetSocketAddress` decode that produced a wildcard/garbage remote address →
> `WSAEADDRNOTAVAIL` (os error 10049) — is the **same family** as DF03/DF04 and
> is addressed:
>
> - **Future-form `AsynchronousSocketChannel.connect(SocketAddress)`** (the path
>   `WsWebSocketContainer.connectToServer` actually uses, lines 306–309) was
>   already made holder-aware on dev by `e6e96b1d` (`native-builtins/
>   phases_late.rs` async connect → `net_phase_e::read_inet_socket_address`).
>   Verified: `connect(sa)` no longer yields os error 10049 (repro
>   `scratch/dfnet/FutureFlowRepro.java` reaches `connect: OK`).
> - **Handler-form `connect(SocketAddress, A, CompletionHandler)`**
>   (`native-io/async_socket.rs::decode_addr`) was STILL holder-blind (read
>   `port`/`hostname`/`addr` directly off the real `InetSocketAddress`, whose
>   state lives in an inner `holder`) → `Err("connect: bad port")` / wildcard.
>   **Fixed** by delegating to the shared holder-aware
>   `socket_channel::decode_socket_address` (now `pub(crate)`), so both async
>   connect paths decode the remote correctly.
>
> **NOT fully resolved — websocket client transport still does not round-trip.**
> Diagnosis (`CRATONVM_DBG_AIO=1`) shows the connect now succeeds, but the
> synthetic async-channel I/O is broken below the address layer:
>   1. The Future-form `connect`/`read`/`write` returned a synthetic `FutureTask`
>      whose REAL `get()` never completes (reads the real `state` field, stuck
>      NEW) → `fConnect.get(timeout)` TimeoutException. (Returning a real
>      `CompletableFuture.completedFuture(...)` fixes *this* layer.)
>   2. The async `read`/`write` decode the `ByteBuffer` via synthetic slots
>      (`get_field(bb,0/1/2)`), wrong for a real `HeapByteBuffer` (slot 0 is
>      `mark`, not the array) → garbage transfer.
>   3. Even with 1+2 fixed, the underlying `fd_table` async socket I/O is
>      inconsistent between `open()` and `open(group)` (one reads back the
>      client's own write; the other transfers zeros), i.e. the two competing
>      async-channel implementations (`phases_late` synthetic + `async_socket`
>      worker-pool) disagree on fd/buffer ownership.
>
> Layers 1–3 are the "complete synthetic async-channel Future/completion model"
> `e6e96b1d` already flagged as a follow-on; that is a substantial unification of
> the two async implementations, tracked separately. Only the address-decode
> family fix is merged here.

**Severity:** Medium-High (breaks the entire websocket client test surface — 24 classes).
**Status on CratonVM:** FAIL. **HotSpot:** PASS.
**Run date:** 2026-06-17
**Binary:** dev `77620f55` (worktree `C:/craton/CratonVM-tcfull`).
**Affected classes (24):** the `org.apache.tomcat.websocket.*` client family, incl.
`TestWsWebSocketContainer(/SSL/GetOpenSessions/TimeoutClient/TimeoutServer)`,
`TestWebSocketFrameClient`, `TestWsPingPongMessages`, `TestWsRemoteEndpoint`,
`TestWsSessionSuspendResume`, `TestWsSubprotocols`,
`TestWsWebSocketContainerSessionExpiry*`,
`org.apache.tomcat.websocket.pojo.TestEncodingDecoding`,
`org.apache.tomcat.websocket.server.TestShutdown / TestSlowClient /
TestClassLoader / TestCloseBug58624 / TestWsServerContainer /
TestWsRemoteEndpointImplServerDeadlock / TestAsyncMessagesPerformance`,
`org.apache.tomcat.security.TestSecurity2018`.

## Symptom

The websocket client (`WsWebSocketContainer.connectToServer`) cannot open the TCP
connection to the just-started embedded server:

```
java.io.IOException: connect failed: The requested address for its context is invalid. (os error 10049)
```

`os error 10049` = Windows `WSAEADDRNOTAVAIL` — the socket layer was handed a
local/remote address that is not valid for a connect (e.g. binding the connect
to a bogus local endpoint, or a `0.0.0.0`/null host, or a port-0 / unresolved
`InetSocketAddress`).

## Root cause (analysis)

This is a **websocket-client connect-path address bug**, distinct from the
server-side selector hang (DF01). The client resolves the target `ws://…` URI
into an `InetSocketAddress` and connects; CratonVM produces an address the OS
rejects with `WSAEADDRNOTAVAIL`. Candidate causes (CratonVM has a history of
`InetSocketAddress` slot/port decode bugs — cf. BUG-C C1/C2):
- the client `SocketChannel.connect`/`bind` is given a wildcard or null local
  address it then tries to use as a source endpoint, or
- the resolved remote `InetSocketAddress` carries port 0 / an unresolved host
  because the holder fields decode wrong.

Because every websocket *client* test fails identically (and server-only
websocket paths fail differently — see DF03), the fault is specifically in the
client `connectToServer` socket-address construction.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$CP = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS="1"; $env:CRATONVM_REAL_AQS="1"; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG="1"
C:\craton\CratonVM-tcfull\target\release\cratonvm.exe -Xmx2g -cp $CP `
  org.junit.runner.JUnitCore org.apache.tomcat.websocket.TestWsWebSocketContainer
```

## Recommendation

**FIX / investigate (networking).** Trace the `InetSocketAddress` /
local-bind-endpoint the client `SocketChannel.connect` receives under
`CRATONVM_REAL_NET_SOCKETS=1`; verify the remote host/port decode and that no
invalid local address is bound before connect. Likely shares machinery with the
already-fixed `read_inet_socket_address` port-decode bug (BUG-C).
