# 19 WebSocket classes fail: sessions never join their group, and a second family writes to a closed socket — OPEN

**Status: OPEN (2026-08-22).** Not root-caused. Two symptom families across the
`org.apache.tomcat.websocket` tree; HotSpot passes the same classes on the same
host and fixture, so this is a CratonVM defect, not an environment gap. Two
hypotheses are DISPROVED below with controls — read those before starting.

**Found by:** the 2026-08-22 full 640-class Tomcat suite on `dev@652956429`
(Azure Linux, real JDK 25, default collector, 4 shards, 300 s cap), then
re-confirmed by re-running every non-passed class **serially** at a 900 s cap.

## The cluster

19 of the 640 classes, all under `org.apache.tomcat.websocket`. The serial
re-run reproduces 15 of the 16 it covers (`TestWsSubprotocols` passed on
re-run — flaky, not clean), so this is not shard contention.

| class | serial re-run |
|---|---|
| `TestWebSocketFrameClient` | FAIL (13 s) |
| `TestWsPingPongMessages` | FAIL (2 s) |
| `TestWsRemoteEndpoint` | FAIL (19 s) |
| `TestWsSessionSuspendResume` | FAIL (4 s) |
| `TestWsSubprotocols` | **OK** on re-run (FAIL in the sweep — flaky) |
| `TestWsWebSocketContainer` | FAIL (382 s) |
| `TestWsWebSocketContainerGetOpenSessions` | FAIL (9 s) |
| `TestWsWebSocketContainerSessionExpiryContainerClient` | FAIL (5 s) |
| `TestWsWebSocketContainerSessionExpiryContainerServer` | FAIL (5 s) |
| `TestWsWebSocketContainerSessionExpirySession` | FAIL (4 s) |
| `TestWsWebSocketContainerTimeoutClient` | FAIL (5 s) |
| `TestWsWebSocketContainerTimeoutServer` | FAIL (7 s) |
| `pojo.TestEncodingDecoding` | FAIL (51 s) |
| `server.TestClassLoader` | HANG (902 s) |
| `server.TestCloseBug58624` | FAIL (207 s) |
| `server.TestShutdown` | FAIL (22 s) |
| `server.TestSlowClient` | FAIL (148 s) |
| `server.TestWsRemoteEndpointImplServerDeadlock` | FAIL |
| `server.TestWsServerContainer` | FAIL |

**HotSpot control, same host and fixture, four sampled classes:**

| class | HotSpot |
|---|---|
| `TestWsSubprotocols` | OK (1 test) |
| `server.TestShutdown` | OK (1 test) |
| `TestWsRemoteEndpoint` | OK (8 tests) |
| `pojo.TestEncodingDecoding` | OK (6 tests) |

## Two families, and they should not be assumed to share a cause

**Family A — the wire.** The visible half: a write to a socket whose peer has
already gone. Signature counts across the 19:

| signature | classes |
|---|---|
| `java.io.IOException: SocketException: write(gathering): Broken pipe (os error 32)` | 5 |
| `java.io.IOException: Message will not be sent because the WebSocket session has been closed` | 4 |
| `java.lang.IllegalStateException: The WebSocket session [0] has been closed and no method (apart from close()) may be called…` | 2 |
| `AssertionError: expected:<3> but was:<1>` | 3 |
| `EOFException` / `TimeoutException` / bare `assertTrue` | 5 |

A representative stack — the close handshake itself failing because the peer
socket is already gone:

```text
java.io.IOException: java.io.IOException: SocketException: write(gathering): Broken pipe (os error 32)
    at WsRemoteEndpointImplBase.sendMessageBlockInternal(WsRemoteEndpointImplBase.java:410)
    at WsSession.sendCloseMessage(WsSession.java:792)
    at WsSession.onClose(WsSession.java:636)
    at WsFrameBase.processDataControl(WsFrameBase.java:377)
    at WsFrameServer.onDataAvailable(WsFrameServer.java:97)
```

`write(gathering)` is CratonVM's own string, from the vectored-write native in
`native-io/src/socket_channel.rs` — so the failing write is
`SocketChannel.write(ByteBuffer[])` on a socket whose peer has already closed.

## A hypothesis that is DISPROVED — do not spend time on it

Every one of the 19 logs carries, immediately before the failure:

```text
WARN cratonvm::gc::guard: a descriptor-aware field access DESTROYED the value it
was handed (G30-1-the-silent-reference-slot-coercion-20260817.md) …
species="primitive-into-reference" access="read" descriptor=L value=Int(0)
class_id=132 index=16
```

19 of 19, 19–26 times each, and `CRATONVM_DBG_LAYOUT=1` resolves `class_id=132`
to `java.util.Properties`. That looks conclusive and is not.

**The control kills it.** `org.apache.tomcat.websocket.pojo.TestPojoEndpointBase`
**PASSES** — `OK (2 tests)` — with **DESTROYED=22**, the same count as the
failing classes. The guard fires at the same rate in passing and failing runs
alike, so it is ambient in this suite and carries no information about this
cluster. (It is a real instrument for a real coercion — see the G30-1 record,
which is separately OPEN — just not this cluster's cause.)

Recorded because the correlation is 100 % and would otherwise be re-derived by
the next person: **a signal present in every failure means nothing until it has
been counted in the passing population.**

## Family B — `getOpenSessions()` returns only the caller

Seven of the 19 are one narrower fact, and it is not "the socket broke" —
**no error or close appears in the log at all** for these. `getOpenSessions()`
simply never contains the session's group-mates.

`TestWsWebSocketContainerGetOpenSessions.testClientAClientAPojoAPojoA` opens two
client sessions on one endpoint and two server sessions on one path, and records
`session.getOpenSessions().size()` from each. It expects `2, 2, 4, 4`; CratonVM
records **`{pojoA=1, client2=1, client1=1}`** — every session sees only itself.
The three `…SessionExpiry*` classes are the same fact (`expected:<3> but
was:<1>`), as is `TestWsSessionSuspendResume`.

That matters because the two sides group by **different key types**
(`WsSession.getSessionMapKey()`): a client session keys on the `localEndpoint`
**instance** (identity `hashCode`/`equals`), a server session on the
`ServerEndpointConfig` **path String** (content-based). Both fail, so the defect
is not in either key's hashing.

### Second hypothesis, also DISPROVED

`WsWebSocketContainer` line 619 groups with
`endpointSessionMap.computeIfAbsent(key, k -> new HashSet<>()).add(wsSession)` —
a single call that would explain both key types at once if it returned a fresh
value instead of the stored one. `probe/CiaProbe.java` exercises exactly that,
with a String key, an identity key, and a longhand `get`/`put` control:

```text
HotSpot   string-key size=3 · identity-key size=3 mapEntries=1 · longhand size=3
CratonVM  string-key size=3 · identity-key size=3 mapEntries=1 · longhand size=3
```

Identical. `computeIfAbsent` is not it.

### The remaining lead, and it is a narrow one

`registerSession` (same file, ~line 609) begins:

```java
protected void registerSession(Object key, WsSession wsSession) {
    if (!wsSession.isOpen()) {
        return;                     // <- never registered, silently
    }
    ...
}
```

If `wsSession.isOpen()` is false at registration time for the 2nd and 3rd
sessions, they are never added to the group and every later `getOpenSessions()`
returns just the one that did register — which is exactly `1` on every counter.
**Confirm or kill that first**: instrument or breakpoint `isOpen()` at the
`registerSession` call in `WsWebSocketContainer.connectToServer` (line 486) and
report `state` for each of the three sessions. It is one measurement and it
splits this sub-cluster cleanly from the `write(gathering)` one.

## Not yet done

* **Not narrowed to client or server side.** The `write(gathering)` failures are
  on the server (`WsFrameServer.onDataAvailable`), but the expiry tests count
  *client* sessions. Running CratonVM as client against HotSpot as server, and
  the reverse, splits this in one pass and is the highest-value next step.
* **Not checked with `--nojit`**, nor per collector.
* **Not bisected.** The 2026-08-14 census had only two websocket classes
  non-passing, but it ran on **Windows** with a different collector pair, so it
  is not a clean baseline — this may be Linux-specific rather than new. A
  `--jdk-only`-style A/B against an older binary on *this* host is what would
  settle "new or always".
* **`server.TestClassLoader` HANGs (902 s)** rather than failing; not
  established whether that is the same cause or a separate one.
* `TestWsWebSocketContainer` takes 382 s and `server.TestCloseBug58624` 207 s
  even while failing, which suggests timeouts being waited out rather than
  errors returned promptly.
