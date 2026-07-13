# ES failure - RestClient node selector connection closed — FIXED

Status: FIXED (2026-07-13)

Date observed: 2026-07-11

## Original report

Focused probe against `dev` commit `d274d898c43a4ca07ac877ba85543d153d2ea83c`,
built as `cratonvm-es-focused-currentdev-20260711-172542`, found
`org.elasticsearch.client.RestClientMultipleHostsIntegTests.testNodeSelector`
failing stably in both JIT modes with:

```text
org.apache.http.ConnectionClosedException: Connection is closed
```

HotSpot passed 4/4; CratonVM (JIT on and off) failed 1/4 with the above.

## Re-investigation (2026-07-13)

Rebuilt from current `dev` (`360d478c7`) in worktree
`C:\craton\CratonVM-restclient-nodeselector-20260713`
(branch `fix/restclient-nodeselector-connectionclosed-20260713`), binary
`cratonvm-restclient-nodeselector-20260713.exe`.

**The originally reported `ConnectionClosedException` no longer reproduces.**
25 single-JVM repro runs of `RestClientMultipleHostsIntegTests` (seed
`B17AC9D3E1F2A0C4`, matching the original repro) against pre-fix current-dev
never hit it. This class exercises the shared HTTP/NIO client-socket stack,
which has had several unrelated fixes land since 2026-07-11 (e.g.
`c0a0450ef` "seed socketLock/impl on bare-allocated java/net/Socket objects",
the WarURLConnection/JarURLConnection content-length fixes, JNDIRealm LDAP
connect fix) — most plausibly one of these incidentally fixed the original
symptom. HotSpot itself was observed to fail `testAsyncRequests` once under
host load (an unrelated timing assertion), confirming this suite is sensitive
to the shared build host's contention — see
`feedback_shared_host_multitenant_confound` — so absence of the exact
original symptom across 25 runs is a reasonably strong (not certain) signal.

**A different, genuinely reproducible bug was found in the same test**
(2/25 pre-fix runs, seed `B17AC9D3E1F2A0C4`, JIT on):

```text
java.io.IOException: ConnectException: finishConnect: Connection refused (os error 10061)
```

`testNodeSelector`'s `stoppedFirstHost` branch does:

```java
try {
    RestClientSingleHostTests.performRequestSyncOrAsync(restClient, request);
    fail("expected to fail to connect");
} catch (ConnectException e) { ... }
```

CratonVM's non-blocking connect path (`sc_finish_connect` in
`native-io/src/socket_channel.rs`) correctly detected `ECONNREFUSED` but threw
a generic `java.io.IOException` whose *message* merely started with the text
`"ConnectException: ..."` rather than an actual `java.net.ConnectException`
instance — so the test's `catch (ConnectException e)` did not match, and the
uncaught `IOException` failed the test. The same message-prefix-instead-of-
real-type pattern existed in two sibling functions: `net_err` in
`native-io/src/net.rs` (plain `java.net.Socket`/datagram natives) and
`re1_connect_socket` in `native-builtins/src/net_phase_e.rs` (blocking
`java.net.Socket.connect()`).

### Fix

Added a real `RuntimeError::ConnectException` variant
(`types/src/error.rs`, mapped to `java/net/ConnectException` in
`vm/src/runtime/exceptions.rs`), and switched all three call sites to throw
it (plus the already-existing typed `RuntimeError::SocketTimeoutException`,
which had the identical bug for connect timeouts) instead of a generic
`IOException` with a fake class-name prefix:

- `native-io/src/socket_channel.rs::map_err` (NIO `SocketChannel.finishConnect()`)
- `native-io/src/net.rs::net_err` (plain `java.net.Socket`/`DatagramSocket` natives)
- `native-builtins/src/net_phase_e.rs::re1_connect_socket` (blocking `Socket.connect()`)

Both `ConnectException` and `SocketTimeoutException` are subclasses of
`IOException`, so no existing `catch (IOException e)` code path is affected —
this only fixes call sites that catch the concrete subtype.

### Verification

- 30/30 clean JIT-on runs and 20/20 clean JIT-off runs of
  `RestClientMultipleHostsIntegTests` post-fix (vs. 2/25 pre-fix JIT-on
  failures reproducing the wrong-exception-type bug).
- Full `client/rest` module (19 classes) run clean post-fix, except one
  transient, non-reproducing `RestClientSingleHostIntegTests` GC-corruption
  warning (3/3 clean on re-run; unrelated to this change — no allocation code
  was touched — consistent with the host's known pre-existing intermittent
  GC-race residuals tracked elsewhere).
- `RestClientSingleHostTests`, `RestClientMultipleHostsTests`,
  `NodeSelectorTests`, `RestClientBuilderTests` all pass.
- Only one other file in the ES tree catches `ConnectException` by type
  (`test/framework/.../ReadinessClientProbe.java`) — narrow blast radius.

Branch `fix/restclient-nodeselector-connectionclosed-20260713`, merged into
`dev`.
