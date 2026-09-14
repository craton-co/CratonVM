# G16-1 — the ServerSocket impl that was never there, and how far `RSslLiveSession` got

> **RECONCILED 2026-08-17 (lane G40) — one inference in §8 no longer holds.**
>
> §8 argues that `plain_socket.rs` is not on the path because *"`plain_socket.rs`'s
> 17 rows have `invocations = 0` under `--jdk-only`"*. **A zero does not support
> that conclusion.** `G33-1` established that `invocations` is a **floor**: it
> counts only registry-resolved dispatches, and the interpreter's intrinsic cache
> and the JIT's thin direct-call helpers dispatch without ever holding a
> `NativeMethodId`. `invocations > 0` still proves a body ran; `invocations == 0`
> proves nothing. To settle whether `plain_socket.rs` is on the path, use
> `owns_slot` plus a behavioural probe, or re-dump under `--nojit` **and**
> `CRATONVM_DISABLE_INTRINSICS=1`, where the counter is exact.
>
> The rest of the record — the measured `ServerSocket.getImpl()` NPE, the field
> writes, `RSslLiveSession` going 0 → 67 check rows, and the nominations — is
> unaffected. `RSslLiveSession` was still red at `9964ca733`. See `INDEX.md` §B.1.

**Status:** BEFORE-MEASURED / AFTER-PENDING-BUILD. **Provenance:** MEAS on both
VMs for every "before" row and every oracle row below; the "after" column of
§0 is the only thing in this record that is not yet measured, and it is marked
so. HotSpot 25.0.3+9-LTS is the oracle throughout. Binary:
`C:/craton/target-fcheck/release/cratonvm.exe`, built 2026-08-17 00:44:49 from
`d87dff06a`+2, run with `--jdk-only`. Probes:
`scratchpad/probe/{G16Sweep,G16Ctx,G16Live}.java` (this lane's; ASCII labels
only).

`RSslLiveSession` is a live-handshake TLS vector that has never passed. It
died on its **first statement**, before any TLS work:

```
CK RSslLiveSession FAILED phase=startup
java.lang.NullPointerException: Cannot invoke "java.net.SocketImpl.setOption(int, Object)"
  because the return value of "java.net.ServerSocket.getImpl()" is null
	at java.net.ServerSocket.setSoTimeout(ServerSocket.java:711)
	at RSslLiveSession.main(RSslLiveSession.java:801)
```

That is a plain-socket defect wearing a TLS vector's clothes. The sweep below
says the same thing **16 more times** on the same object, and then shows that
everything on the far side of that one call already works.

---

## 0. The headline

| vector | before (MEASURED) | after |
|---|---|---|
| `RSslLiveSession` | `FAILED phase=startup`, NPE on `ServerSocket.setSoTimeout`, **0 checks reached** | **NOT YET MEASURED** — no rebuild was available to this lane (§8). The nearest thing to evidence is §4.5: with that one call removed, everything behind it already works. |
| `RSslNullSession` | PASS, 89 checks | not re-run — see §8 |
| `RJdkNet` | PASS, 81 checks | not re-run — see §8 |
| `RJdkAsyncChannel` | PASS, 141 checks | not re-run — see §8 |

| surface | before (MEASURED) | after |
|---|---|---|
| `SSLServerSocket` surface (58 rows swept) | **17 rows NPE** on a null `getImpl()`; 28 rows diverge in all | not yet measured |
| `SSLSessionContext` family (43 rows swept) | **12 rows diverge** (5 distinct defects) | not yet measured |
| live TLS 1.3 handshake with `setSoTimeout` removed (§4.5) | **15 of 16 rows already match HotSpot** on the PRE-FIX binary | — |


## 1. Why `getImpl()` was null — a field-slot collision, not a missing native

`t27_tls::create_ssl_server_socket` hands back an instance of the **real**
`javax.net.ssl.SSLServerSocket` and then stores its own three ints in object
slots 0/1/2 (`SSS_LISTENER_ID = 0`, `SSS_LOCAL_PORT = 1`, `SSS_CLOSED = 2`),
plus `set_field(obj, 3, Value::Object(None))`.

Those are not free slots. `javap -p java.net.ServerSocket` on JDK 25:

```text
0  private final    java.net.SocketImpl              impl
1  private volatile boolean                          created
2  private volatile boolean                          bound
3  private volatile boolean                          closed
4  private final    java.lang.Object                 socketLock
5  private volatile java.util.Set<SocketOption<?>>   options
```

So, row by row:

| write | lands on | effect |
|---|---|---|
| `set_field(obj, 0, Int(listener_id))` | `impl` — a **reference** field | **dropped by the layout guard**; `impl` stays `null` |
| `set_field(obj, 1, Int(local_port))` | `created` (boolean) | `created` becomes "true" for any non-zero port |
| `set_field(obj, 2, Int(closed))` | `bound` (boolean) | **`close()` sets `bound = true`** |
| `set_field(obj, 3, Object(None))` | `closed` (boolean) | dropped — a reference into an int slot |

The first row is the answer to "why was `getImpl()` null": nothing failed and
nothing threw. The int write into a reference-typed slot was silently
discarded, exactly as this file's own `SockSide`/`SsSide` doc comments predict
for the plain classes, and `impl` was never assigned by anything else because
the real `<init>` never ran.

The third row is visible in the sweep and is worth its own line. On CratonVM
`ssl.bound.isBound` reads `false` and `ssl.closed.isBound` reads `true` — a
`ServerSocket` that becomes bound by being closed. It is `SSS_CLOSED = 2`
landing on `bound`.

**Correction to this file, made here.** The `SS_*` block comment in
`net_phase_e.rs` says "real `ServerSocket` slot 0 is `boolean created`, not the
int port". It is `impl`. `created` is slot 1.

## 2. The oracle table — the whole family, four lifecycle points

Probe `G16Sweep`. Every row is a value or an exception class **plus its exact
message, transcribed**; `null` prints as `null` and the empty string as `""`.
Two control sections (a plain `ServerSocket` before bind, after bind, after
accept, after close, and the accepted `Socket`) are not reproduced in full
because **CratonVM already matches HotSpot on every one of their rows** —
`--jdk-only` sets `io.real_net_sockets`, `register_re2_server_socket`
early-returns, and the plain classes run real bytecode over a real
`NioSocketImpl`. The two exceptions found there are in §5.

The sections that diverge, in full:

| row | HotSpot 25.0.3+9 | CratonVM --jdk-only (before) |
|---|---|---|
| `ssl.bound.class` | `"sun.security.ssl.SSLServerSocketImpl"` | `"javax.net.ssl.SSLServerSocket"` |
| `ssl.bound.isBound` | `true` | `false` |
| `ssl.bound.isClosed` | `false` | `false` |
| `ssl.bound.getLocalPort` | `60553` | `60565` |
| `ssl.bound.getInetAddress` | `"127.0.0.1"` | `null` |
| `ssl.bound.getLocalSocketAddress` | `"bound"` | `null` |
| `ssl.bound.getSoTimeout` | `0` | **NPE (getImpl() is null)** |
| `ssl.bound.setSoTimeout.20000` | `"ok"` | **NPE (getImpl() is null)** |
| `ssl.bound.getSoTimeout.after` | `20000` | **NPE (getImpl() is null)** |
| `ssl.bound.setSoTimeout.negative` | `IllegalArgumentException` "timeout < 0" | `IllegalArgumentException` "timeout < 0" |
| `ssl.bound.getReuseAddress` | `false` | **NPE (getImpl() is null)** |
| `ssl.bound.setReuseAddress.true` | `"ok"` | **NPE (getImpl() is null)** |
| `ssl.bound.getReceiveBufferSize` | `true` | **NPE (getImpl() is null)** |
| `ssl.bound.setReceiveBufferSize.8192` | `"ok"` | **NPE (getImpl() is null)** |
| `ssl.bound.setReceiveBufferSize.zero` | `IllegalArgumentException` "negative receive size" | `IllegalArgumentException` "negative receive size" |
| `ssl.bound.supportedOptions.hasSO_RCVBUF` | `true` | **NPE (impl is null)** |
| `ssl.bound.getOption.SO_RCVBUF` | `"int>0"` | **NPE (getImpl() is null)** |
| `ssl.bound.getOption.SO_REUSEADDR` | `true` | **NPE (getImpl() is null)** |
| `ssl.bound.getOption.SO_REUSEPORT` | `UnsupportedOperationException` "'SO_REUSEPORT' not supported" | **NPE (getImpl() is null)** |
| `ssl.bound.getOption.IP_TOS` | `"0"` | **NPE (getImpl() is null)** |
| `ssl.bound.setOption.SO_REUSEADDR.true` | `true` | **NPE (getImpl() is null)** |
| `ssl.bound.setOption.null.option` | **NPE** | **NPE** |
| `ssl.bound.getOption.null.option` | **NPE** | **NPE** |
| `ssl.bound.getChannel` | `null` | `null` |
| `ssl.bound.bind.again` | `SocketException` "Already bound" | `SocketException` "Already bound" |
| `ssl.bound.toString.startsWith` | `true` | `true` |
| `ssl.bound.getEnabledCipherSuites.nonempty` | `true` | `AbstractMethodError` "method javax/net/ssl/SSLServerSocket.getEnabledCipherSuites()[Ljava/lang/String; has no Code attribute" |
| `ssl.bound.getNeedClientAuth` | `false` | `false` |
| `ssl.bound.getWantClientAuth` | `false` | `false` |
| `ssl.bound.getUseClientMode` | `false` | `AbstractMethodError` "method javax/net/ssl/SSLServerSocket.getUseClientMode()Z has no Code attribute" |
| `ssl.bound.getEnableSessionCreation` | `true` | `AbstractMethodError` "method javax/net/ssl/SSLServerSocket.getEnableSessionCreation()Z has no Code attribute" |
| `ssl.closed.class` | `"sun.security.ssl.SSLServerSocketImpl"` | `"javax.net.ssl.SSLServerSocket"` |
| `ssl.closed.isBound` | `true` | `true` |
| `ssl.closed.isClosed` | `true` | `true` |
| `ssl.closed.getLocalPort` | `60553` | `60565` |
| `ssl.closed.getInetAddress` | `"127.0.0.1"` | **NPE (getImpl() is null)** |
| `ssl.closed.getLocalSocketAddress` | `"bound"` | **NPE (getImpl() is null)** |
| `ssl.closed.getSoTimeout` | `SocketException` "Socket is closed" | `SocketException` "Socket is closed" |
| `ssl.closed.setSoTimeout.20000` | `SocketException` "Socket is closed" | `SocketException` "Socket is closed" |
| `ssl.closed.getSoTimeout.after` | `SocketException` "Socket is closed" | `SocketException` "Socket is closed" |
| `ssl.closed.setSoTimeout.negative` | `SocketException` "Socket is closed" | `SocketException` "Socket is closed" |
| `ssl.closed.getReuseAddress` | `SocketException` "Socket is closed" | `SocketException` "Socket is closed" |
| `ssl.closed.setReuseAddress.true` | `SocketException` "Socket is closed" | `SocketException` "Socket is closed" |
| `ssl.closed.getReceiveBufferSize` | `SocketException` "Socket is closed" | `SocketException` "Socket is closed" |
| `ssl.closed.setReceiveBufferSize.8192` | `SocketException` "Socket is closed" | `SocketException` "Socket is closed" |
| `ssl.closed.setReceiveBufferSize.zero` | `IllegalArgumentException` "negative receive size" | `IllegalArgumentException` "negative receive size" |
| `ssl.closed.supportedOptions.hasSO_RCVBUF` | `true` | **NPE (impl is null)** |
| `ssl.closed.getOption.SO_RCVBUF` | `SocketException` "Socket is closed" | `SocketException` "Socket is closed" |
| `ssl.closed.getOption.SO_REUSEADDR` | `SocketException` "Socket is closed" | `SocketException` "Socket is closed" |
| `ssl.closed.getOption.SO_REUSEPORT` | `SocketException` "Socket is closed" | `SocketException` "Socket is closed" |
| `ssl.closed.getOption.IP_TOS` | `SocketException` "Socket is closed" | `SocketException` "Socket is closed" |
| `ssl.closed.setOption.SO_REUSEADDR.true` | `SocketException` "Socket is closed" | `SocketException` "Socket is closed" |
| `ssl.closed.setOption.null.option` | **NPE** | **NPE** |
| `ssl.closed.getOption.null.option` | **NPE** | **NPE** |
| `ssl.closed.getChannel` | `null` | `null` |
| `ssl.closed.bind.again` | `SocketException` "Socket is closed" | `SocketException` "Socket is closed" |
| `ssl.closed.toString.startsWith` | `true` | **NPE (impl is null)** |

### 2.1 The finding that decided the repair

Cross-compare the oracle against **itself**, `plain.unbound` vs `ssl.bound`:

```text
DIFF class                 G16Sweep$PSS          sun.security.ssl.SSLServerSocketImpl
DIFF isBound               false                 true
DIFF getLocalPort          -1                    60553
DIFF getInetAddress        null                  "127.0.0.1"
DIFF getLocalSocketAddress null                  "bound"
DIFF bind.again            "ok"                  SocketException "Already bound"
```

and `plain.closed` vs `ssl.closed`:

```text
DIFF class                 G16Sweep$PSS          sun.security.ssl.SSLServerSocketImpl
DIFF getLocalPort          60551                 60553
```

Nothing else differs. **Every option row of a bound `SSLServerSocketImpl` is
identical to the same row on an UNBOUND plain `ServerSocket`** — value,
exception class and message alike. Six rows separate them, and all six are
about *identity* (which class, which address, which port, bound or not), not
about options.

That is not a coincidence to be exploited carelessly; it is the JSSE design.
`SSLServerSocketImpl` does not override any of these — they are
`java.net.ServerSocket`'s own bodies reading `SocketImpl`, and an unbound
platform impl answers them the same way a bound one does.

## 3. The repair — borrow the JDK's answers instead of transcribing them

`net_phase_e.rs`, new `register_ssl_server_socket_options` (RE.6b), called from
`register_phase_e_networking`. Nine methods registered on
`javax/net/ssl/SSLServerSocket`, each forwarded to a real, **unbound**
`java.net.ServerSocket` created on first use and remembered per receiver as a
global-root handle:

```text
getSoTimeout ()I                setSoTimeout (I)V
getReuseAddress ()Z             setReuseAddress (Z)V
getReceiveBufferSize ()I        setReceiveBufferSize (I)V
supportedOptions ()Ljava/util/Set;
getOption (Ljava/net/SocketOption;)Ljava/lang/Object;
setOption (Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/net/ServerSocket;
```

Three things this buys that a transcription would not:

* the two **argument-check orderings that disagree between siblings** come out
  right for free. `setSoTimeout` tests closed-ness first (`ssl.closed
  .setSoTimeout.negative` is `SocketException`, not `IllegalArgumentException`)
  while `setReceiveBufferSize` tests the argument first (`ssl.closed
  .setReceiveBufferSize.zero` is `IllegalArgumentException "negative receive
  size"` even on a closed socket). Two adjacent setters, opposite orders.
* `SO_REUSEPORT` -> `UnsupportedOperationException("'SO_REUSEPORT' not
  supported")` on Windows, `getOption(null)` -> a **message-less** NPE, and
  `getOption(SO_REUSEADDR)` -> `true` while `getReuseAddress()` -> `false` on
  the very same socket. That last pair is genuinely what HotSpot answers, on
  the plain class too; no hand-written pair would have produced it.
* the closed state needs no bookkeeping of its own. Before each forward, the
  SSL socket's own `isClosed()` is asked (t27_tls's native, the authority) and
  the delegate is closed to match, once. The real bytecode then produces
  `SocketException "Socket is closed"` with the right ordering everywhere.

`setOption` returns `this`, not the delegate — handing back the delegate would
let `ss.setOption(..).accept()` accept on the wrong listener.

**Deliberately NOT registered**, and this is the interesting half:
`isBound`, `getInetAddress`, `getLocalSocketAddress`, `toString`. They need the
bind ADDRESS, which only `create_ssl_server_socket` sees. And they have to move
**together**: `ServerSocket.toString()` short-circuits to the constant
`"ServerSocket[unbound]"` while `isBound()` is false, which is the only reason
`ssl.bound.toString` currently AGREES with HotSpot. Registering `isBound` on
its own — the obvious one-row fix, and the oracle does say `true` — flips that
agreeing row into `NullPointerException: ... "this.impl" is null`. A unit test
(`ssl_server_socket_bound_identity_rows_are_not_registered_piecemeal`) fails if
a later change lands one without the other.

### 3.1 The client twin already had all of this

`javax/net/ssl/SSLSocket` carries 39 registrations, 35 of them from
`phases_late/ssl_security.rs`, and they include exactly the surface that was
missing on the server class — `setSoTimeout`/`getSoTimeout` (4931/4909),
`isBound` (4838), `getInetAddress` (4961), `getLocalSocketAddress`,
`isConnected`, `isClosed`. `ssl_security.rs:4822`'s own comment records the
same defect shape being found there earlier: *"`isBound()` had NO registration
on this class, so the real `java.net.Socket.isBound()` bytecode ran and read
the `state` word out of a synthetic object whose slots this file writes `Int`s
into"*.

So this is not a new species. The client half of the pair was repaired; the
**server half was never given the same treatment**, and nothing pointed at it
until a vector opened a listener. Note also that `ssl_security.rs` registers
LATER than `net_phase_e.rs` and scopes its work to a single `ssl_sock` class
constant — it does not touch `javax/net/ssl/SSLServerSocket`, so RE.6b is not
shadowed by it today. A future consolidation that moves RE.6b next to its twin
in `ssl_security.rs` is reasonable and would be safe on ordering grounds, but
must be re-checked against the dump, not against the source.

## 4. The second family this lane measured — `javax.net.ssl.SSLSessionContext`

A sibling lane nominated three items against this file (N1/N2/N3). Two are
about `SSLSessionContext`, so the family was swept rather than the rows.
Probe `G16Ctx`: no network, no handshake, one `SSLContext.getInstance("TLS")`
that has been `init`-ed. Client and server contexts answered identically on
both VMs, so only the server half is reproduced.

| row | HotSpot 25.0.3+9 | CratonVM --jdk-only (before) |
|---|---|---|
| `server.isNull` | `false` | `false` |
| `server.getSessionCacheSize` | `20480` | **`0`** |
| `server.getSessionTimeout` | `86400` | **`0`** |
| `server.getIds.isNull` | `false` | `false` |
| `server.getIds.hasMoreElements` | `false` | `false` |
| `server.getSession.null` | `NullPointerException` "session id cannot be null" | **`null`** |
| `server.getSession.empty` | `null` | `null` |
| `server.getSession.unknown` | `null` | `null` |
| `server.setSessionCacheSize.64` | `"ok"` | `"ok"` |
| `server.getSessionCacheSize.after` | `64` | `64` |
| `server.setSessionCacheSize.negative` | `IllegalArgumentException` null | **`IllegalArgumentException` "negative session cache size: -1"** |
| `server.setSessionCacheSize.zero` | `"ok"` | `"ok"` |
| `server.getSessionCacheSize.afterZero` | `0` | `0` |
| `server.setSessionTimeout.30` | `"ok"` | `"ok"` |
| `server.getSessionTimeout.after` | `30` | `30` |
| `server.setSessionTimeout.negative` | `IllegalArgumentException` null | **`IllegalArgumentException` "negative session timeout: -1"** |
| `server.setSessionTimeout.zero` | `"ok"` | `"ok"` |
| `server.getSessionTimeout.afterZero` | `0` | `0` |
| `server.sameObjectTwice` | `"see identity rows"` | `"see identity rows"` |
| `identity.serverContext.sameInstance` | `true` | **`false`** |
| `identity.serverContext.equals` | `true` | **`false`** |
| `identity.roundTrip` | `77` | `77` |
| `identity.clientVsServer.same` | `false` | `false` |
| `identity.clientUnaffected` | `0` | `0` |

Five divergences, all inside `net_phase_e.rs`, all fixed here:

1. **`getSessionCacheSize()` = 20480, not 0** and **`getSessionTimeout()` =
   86400, not 0**, on a context nobody has configured. `SscSide`'s `#[derive(
   Default)]` gave 0/0 on the reading that 0 is this API's spelling of
   "unlimited"/"no expiry". It is — but it is also a value a caller can SET,
   and setting it reads back as 0 on HotSpot too (`getSessionCacheSize
   .afterZero` = 0 on both VMs). So the two states are distinguishable and the
   old default reported the *configured* one as the *initial* one. Replaced
   with a hand-written `Default` and two named constants.
2. **`getSession(null)` must throw** `NullPointerException("session id cannot
   be null")`. It returned `null`. This is the whole of the family where a null
   argument is not just a miss: `getSession(new byte[0])` and
   `getSession(unknown)` both answer `null` and both already agreed. Returning
   `null` for a null id told the caller its lookup had MISSED where the JDK
   refuses the call.
3. **`setSessionCacheSize(-1)` and `setSessionTimeout(-1)` throw
   `IllegalArgumentException` with a NULL message.** Both sites had invented
   `"negative session cache size: -1"` / `"negative session timeout: -1"`.
   HANDOFF §5 again: a message cannot be derived, only transcribed, and here
   the transcription is *no message at all* (`RuntimeError::
   IllegalArgumentException { message: String::new() }` is this crate's marker
   for a null `getMessage()`).
4. **`ctx.getServerSessionContext() == ctx.getServerSessionContext()` is
   `true`.** It read `false`: every call minted a fresh carrier.
   `ssc_owner_table` already made the *state* round-trip across two different
   carriers, which is exactly why the values agreed while the identity did not
   — `identity.roundTrip` reads 77 on both VMs. Identity is not cosmetic here:
   a caller that caches the context and later compares it, or uses it as a map
   key, accumulates one entry per call. Fixed by minting the carrier once per
   `(SSLContext, side)` and holding it as a global root.
5. `ssc_bind` is now `pub(crate)` (the sibling lane's request), so a carrier
   minted outside this file can be bound to its owning `SSLContext` instead of
   being filed under `SSC_TAG_ORPHAN`.

**Not** taken from N1: the "real session table" behind `getIds()`. On a fresh
context the oracle answers `getIds().hasMoreElements() = false` and
`getSession(unknown) = null` — which is what this VM already answers, because
rustls owns the cache and exposes no enumeration. The rows that would
distinguish a real table from an empty one only exist *after* a handshake, and
this lane could not measure them (see §6). Left open, unchanged.

## 4.5 What is on the other side of `setSoTimeout` — MEASURED

The one thing that could still not be measured is whether the fix makes
`RSslLiveSession` progress. So probe `G16Live` was written to answer the next
question instead: it reproduces the vector's startup and first exchange
**with the `ss.setSoTimeout(20000)` line removed** — the single call the vector
dies on and the only one RE.6b unblocks — and runs it on the SAME pre-fix
binary. Self-signed RSA identity, loopback listener, accept thread, client
handshake, request/response.

| row | HotSpot 25.0.3+9 | CratonVM `--jdk-only` (pre-fix binary) |
|---|---|---|
| `live.client.startHandshake` | `"ok"` | `"ok"` |
| `live.client.session.cipher` | `"TLS_AES_256_GCM_SHA384"` | `"TLS_AES_256_GCM_SHA384"` |
| `live.client.session.protocol` | `"TLSv1.3"` | `"TLSv1.3"` |
| `live.client.session.id.length` | `32` | `32` |
| `live.client.session.isValid` | `true` | `true` |
| `live.client.session.context.isNull` | `false` | `false` |
| `live.client.session.peerPrincipal` | `"CN=localhost"` | `"CN=localhost"` |
| `live.client.session.peerCerts.length` | `1` | `1` |
| `live.client.session.packetBufferSize` | `16709` | `16709` |
| `live.client.session.applicationBufferSize` | `16676` | **`16384`** |
| `live.exchange` | `"body-ok"` | `"body-ok"` |
| `live.server.error` | `"none"` | `"none"` |
| `live.server.session.cipher` | `"TLS_AES_256_GCM_SHA384"` | `"TLS_AES_256_GCM_SHA384"` |

**A complete live TLS 1.3 handshake over a CratonVM `SSLServerSocket` already
works under `--jdk-only`, in both directions, with a real body exchanged, and
15 of 16 rows match HotSpot exactly.** `ServerSocket.setSoTimeout` really was
standing alone in front of all of it. That is evidence — not proof — that
RE.6b unblocks the vector; only a rebuild settles it (§8).

It also settles half of the sibling lane's N2, and corrects the other half.
`getPacketBufferSize` is **already right** (16709 on both). Only
`getApplicationBufferSize` diverges: 16676 vs 16384. And `t27_tls.rs:17020`
already knows — the 16384 is a *deliberate, documented under-report*, with the
reasoning that raising a buffer-size constant without auditing every
`BUFFER_OVERFLOW` path is how a constant becomes an outage. Note also that a
single constant cannot be right in both states: the same block records 16704
for a never-connected session against 16676 after TLS 1.3, because real
`SSLSessionImpl` derives it from the packet size minus the negotiated suite's
record expansion. See NOM-6.

## 5. Two more measured divergences this lane did not fix

* `plain.accepted.getReuseAddress` — HotSpot `false`, CratonVM `true`, and
  identically for `getOption(SO_REUSEADDR)`. This is a **real** accepted
  `Socket` on the real-bytecode path, so it is `sun/nio/ch/Net`'s option
  plumbing in `native-io`, not this file. Every other row of the accepted and
  closed-accepted socket matches.
* Three `AbstractMethodError`s on the SSL socket itself:
  `getEnabledCipherSuites()`, `getUseClientMode()`, `getEnableSessionCreation()`
  — "has no Code attribute", because `javax.net.ssl.SSLServerSocket` is
  abstract and the object is an instance of it directly. `t27_tls` owns that
  class's SSL surface. Nominated, not touched.

## 6. N3 — the shadowing registrar, confirmed from this lane's own dump

The sibling lane's N3 said a comment in `net_phase_e.rs` rules out a shadowing
registrar by checking only `register_one`. It does, and the comment is wrong.
`--dump-native-registry` under `--jdk-only`, both carrier classes, identical
rows:

```text
getCipherSuite         owns_slot=false  net_phase_e.rs:8315
getCipherSuite         owns_slot=TRUE   http_url_connection.rs:307  overwrote=bridge
getServerCertificates  owns_slot=false  net_phase_e.rs:8323
getServerCertificates  owns_slot=TRUE   http_url_connection.rs:283  overwrote=bridge
getLocalCertificates   owns_slot=false  net_phase_e.rs:8346
getLocalCertificates   owns_slot=TRUE   http_url_connection.rs:301  overwrote=bridge
getPeerPrincipal       owns_slot=false  net_phase_e.rs:8364
getPeerPrincipal       owns_slot=TRUE   http_url_connection.rs:325  overwrote=bridge
getLocalPrincipal      owns_slot=false  net_phase_e.rs:8377
getLocalPrincipal      owns_slot=TRUE   http_url_connection.rs:351  overwrote=bridge
getSSLSession          owns_slot=TRUE   net_phase_e.rs:8395
```

**Five of the six bodies in `net_phase_e::register_https_session_accessors` are
dead.** The shadow does not come from `register_one` at all; it comes from a
second function in the same other file **with the same name as this one**,
`http_url_connection.rs:404`. Only `getSSLSession` survives, and only because
the other file does not register it.

The comment has been rewritten with the dump pasted into it. The general
lesson is worth stating plainly, because the old comment had it exactly
backwards: **a shadowing registrar is not ruled out by grepping one function
for one name.** Only the dump settles it.

## 7. Which body runs — the registry check for this lane's own work

Before: `java/net/ServerSocket` and `java/net/Socket` have **zero** rows in the
`--jdk-only` dump. `register_re2_server_socket` and `register_re1_socket`
early-return on `vmflags().io.real_net_sockets`, which is default-ON. So the
plain classes are real bytecode in both modes, and RE.6b's delegate calls reach
real bytecode too. Rows that DO run, with `owns_slot=true, invocations=1`:

```text
javax/net/ssl/SSLContext.getServerSocketFactory        net_phase_e.rs:13240
javax/net/ssl/SSLServerSocketFactory.createServerSocket(IILjava/net/InetAddress;)
                                                       t27_tls.rs:5844
javax/net/ssl/SSLServerSocket.isClosed                 t27_tls.rs:5892
```

The nine RE.6b names are disjoint from the twelve `t27_tls::
register_sslserversocket` claims on the same class. That matters and is not
obvious from the source: `register_phase_e_networking` runs at lib.rs:18688 and
`register_t27_natives` at lib.rs:18731, so a name in both files would be
silently taken over by t27's later body. Unit test
`ssl_server_socket_option_registrar_leaves_the_t27_owned_names_alone` fails if
one is ever added.

## 8. What this lane did NOT do

* **It did not verify the fix by running the binary.** The binary available to
  this lane was built at 00:44:49 from `d87dff06a`+2, before these edits, and
  this lane is forbidden to run `cargo build`/`check`/`test` (the orchestrator
  owns the target dir). Every "before" row here is measured; the "after" is
  not. Per HANDOFF §5 — *a green build proves you broke nothing, not that you
  did something* — nothing in this record should be read as a claim that
  `RSslLiveSession` passes, or that it gets further than `setSoTimeout`. **The
  next lane's first act should be to rebuild and re-run §0's four vectors.**
* **It did not make `setSoTimeout` reach `accept()`.** The value now round-trips
  correctly and stops throwing, but `t27_tls`'s `SSLServerSocket.accept` parks
  in `rustls_server_accept`'s poll loop unboundedly and consults no timeout at
  all — before this change it consulted none either, because the setter threw.
  A caller that relies on `accept()` raising `SocketTimeoutException` still
  will not get one. `RSslLiveSession.serve` catches and retries that exception,
  so it tolerates either behaviour; something else may not. Nominated as part
  of NOM-1's file.
* It did not fix the root cause. The right repair is for
  `create_ssl_server_socket` to stop writing ints into `impl`/`created`/`bound`
  and give the object a real `SocketImpl`; that is `t27_tls.rs`, not this
  lane's file. RE.6b routes around the null `impl`; it does not populate it.
* It did not touch `isBound` / `getInetAddress` / `getLocalSocketAddress` /
  `toString` on the SSL socket (§3), the three `AbstractMethodError` rows
  (§5), or `plain.accepted.getReuseAddress` (§5).
* It did not build a real session table behind `getIds()` (§4).
* It did not fix `SSLSession.getApplicationBufferSize` (measured in §4.5,
  nominated as NOM-6). The value lives in `t27_tls.rs`, behind a documented
  under-report rationale that deserves the owning lane's judgement.
* It did not touch `plain_socket.rs` or `tls_impl.rs`. Neither is on the path:
  `plain_socket.rs`'s 17 rows have `invocations = 0` under `--jdk-only`, and
  `tls_impl.rs` registers nothing that appears in the dump at all.

## 9. NOMINATIONS

**NOM-1 — `t27_tls.rs`, `create_ssl_server_socket` (~5764). The root cause.**
Slots 0/1/2 of a real `javax.net.ssl.SSLServerSocket` are `impl` / `created` /
`bound` (§1). The three `ctx.set_field` calls there are, respectively, a write
that is silently dropped, a write that turns `created` on, and a write that
makes `close()` set `bound`. `ssl_server_socket_state`'s side table is already
the authority for all three values and the field reads are only
`unwrap_or_else` fallbacks, so **deleting the four field writes should be
behaviour-preserving for t27's own natives and removes the corruption**. Verify
with the dump and the `G16Sweep` probe, not by reading.

**NOM-2 — `t27_tls.rs`. The bind address, and `isBound`/`toString` as one
change.** `create_ssl_server_socket` is the only place that knows the bind
address. Recording it beside `local_port` would let four rows be answered:
`isBound` -> `true`, `getInetAddress` -> the bind address,
`getLocalSocketAddress` -> non-null, `toString` -> `ServerSocket[addr=/<ip>,
localport=<port>]`. **They must land together** — see §3; `isBound` alone
breaks `toString`.

**NOM-3 — `t27_tls.rs`, `register_sslserversocket`. Three
`AbstractMethodError`s.** `getEnabledCipherSuites()` (oracle: non-empty),
`getUseClientMode()` (`false`), `getEnableSessionCreation()` (`true`). The
class is abstract; there is no body to inherit.

**NOM-4 — `http_url_connection.rs`, `register_https_session_accessors:404`.**
Five HTTPS session accessors are served from there, not from `net_phase_e.rs`
(§6). Any future fix to `getCipherSuite` / `getServerCertificates` /
`getLocalCertificates` / `getPeerPrincipal` / `getLocalPrincipal` must be made
in that file. Worth considering whether the two same-named registrars should be
collapsed into one.

**NOM-5 — `native-io`, `sun/nio/ch/Net` option plumbing.**
`Socket.getReuseAddress()` and `getOption(SO_REUSEADDR)` on an ACCEPTED socket
read `true`; HotSpot reads `false` (§5). Measured on the real-bytecode path
under `--jdk-only`.

**NOM-6 — `t27_tls.rs:17050` (and its `--synthetic-jdk` twin `tls.rs:1370`).
`SSLSession.getApplicationBufferSize()`.** MEASURED post-handshake over a live
TLS 1.3 loopback session (§4.5): HotSpot 16676, CratonVM 16384.
`getPacketBufferSize` needs nothing — 16709 on both. The existing comment's
own numbers say a constant cannot serve both states (16704 unconnected, 16676
after TLS 1.3), so the fix is to derive it from the negotiated suite, not to
retune the constant. The comment's caution about `BUFFER_OVERFLOW` paths is
the reason this lane did not simply raise the number from outside the file.

**Not nominated, deliberately:** nothing about `regression-suite/`. The vector
is correct as written; it died on CratonVM's side of the line.

## 9.5 How the next lane verifies this in five minutes

```bash
export JAVA_HOME="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
cd C:/craton/cvm-mergecheck
# 1. the vector that was red
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cratonvm.exe> --java-home "$JAVA_HOME"     --jdk-only -cp regression-suite/build RSslLiveSession
# 2. the three that were green: 89 / 81 / 141 checks
for V in RSslNullSession RJdkNet RJdkAsyncChannel; do ... ; done
# 3. the two probes, diffed against the oracle, line endings normalised
<cratonvm.exe> --java-home "$JAVA_HOME" --jdk-only -cp <probe> G16Sweep
<cratonvm.exe> --java-home "$JAVA_HOME" --jdk-only -cp <probe> G16Ctx
# 4. and the thing reading cannot settle
<cratonvm.exe> --java-home "$JAVA_HOME" --jdk-only     --dump-native-registry reg.json -cp <cp> RSslLiveSession   # flags BEFORE the class
```

Expected in `reg.json` after this change: nine rows on
`javax/net/ssl/SSLServerSocket` from `net_phase_e.rs`, all with
`owns_slot=true` and `overwrote=null`, and `setSoTimeout` with
`invocations >= 1`. If `owns_slot` is false on any of them, something in
`t27_tls.rs` or `ssl_security.rs` has grown a same-named registration and RE.6b
is dead code — §6 is that failure, already realised once in this same file.

## 10. Files this lane touched

* `native-builtins/src/net_phase_e.rs` — RE.6b (§3), the `SSLSessionContext`
  family (§4), the corrected `SS_*` layout comment (§1) and the corrected
  registration-order comment (§6), and four unit tests:
  `ssl_session_context_defaults_are_the_measured_hotspot_values`,
  `ssl_server_socket_inherited_option_surface_is_registered`,
  `ssl_server_socket_option_registrar_leaves_the_t27_owned_names_alone`,
  `ssl_server_socket_bound_identity_rows_are_not_registered_piecemeal`.
* `docs/known-issues/jdk-only/G16-1-...md` — this record.

Nothing else. `plain_socket.rs` and `tls_impl.rs` are unchanged.
