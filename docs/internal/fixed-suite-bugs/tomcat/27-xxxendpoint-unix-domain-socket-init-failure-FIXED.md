# `TestXxxEndpoint.testUnixDomainSocket` — protocol handler init failure

**Status:** ✅ **FIXED** (2026-07-27). `org.apache.tomcat.util.net.TestXxxEndpoint`
now reports `OK (3 tests)` on CratonVM, matching HotSpot. Verified over 4/4
runs (3× JIT on, 1× `--nojit`), ~47–68 s each.

## Symptom (as originally reported)

```
org.apache.catalina.LifecycleException: Protocol handler initialization failed
	at org.apache.catalina.connector.Connector.initInternal(Connector.java:1279)
	at org.apache.catalina.core.StandardService.initInternal(StandardService.java:543)
Caused by: java.lang.UnsupportedOperationException: Unix domain sockets are not supported on this platform
	at org.apache.tomcat.util.net.NioEndpoint.initServerSocket(NioEndpoint.java:315)
	at org.apache.tomcat.util.net.AbstractEndpoint.bindWithCleanup(AbstractEndpoint.java:2179)
```

`testUnixDomainSocket` sets `unixDomainSocketPath` on the connector, so
`NioEndpoint.initServerSocket()` takes its AF_UNIX branch:

```java
SocketAddress sa = UnixDomainSocketAddress.of(getUnixDomainSocketPath());
serverSock = ServerSocketChannel.open(StandardProtocolFamily.UNIX);
serverSock.bind(sa, getAcceptCount());
```

The test then connects a `SocketChannel.open(StandardProtocolFamily.UNIX)`
client **in the same JVM**, writes `OPTIONS * HTTP/1.0`, and asserts the reply
starts with `HTTP/1.1 200`.

## Root causes (two, in sequence)

### 1. `UnixDomainSocketAddress` was stubbed to throw, and UDS channels did not exist

`native-builtins/src/phases_late/net_channels.rs` registered natives for
`java.net.UnixDomainSocketAddress.of(String)` / `of(Path)` / `getPath()` that
unconditionally threw
`UnsupportedOperationException("Unix domain sockets are not supported on this platform")`
— the exact text in the `Caused by:` above. That override was **not**
synthetic-JDK-only: it applied in real-JDK mode too, where the genuine
`java.net.UnixDomainSocketAddress` is an ordinary (non-native) class needing
nothing CratonVM lacks. A probe confirmed every prerequisite already worked:
`Path.of`, `path.getFileSystem() == FileSystems.getDefault()`, and
`fs.getClass().getModule() == Object.class.getModule()` all behave as on
HotSpot.

Deleting the stub alone was not enough. `ServerSocketChannel.open()` /
`SocketChannel.open()` are **natively intercepted** by CratonVM
(`native-io/src/socket_channel.rs`), which builds its own channel objects
backed by the `tcp_registry`; only those are drivable by CratonVM's
read/write/selector natives. The JDK-16 `open(ProtocolFamily)` overloads were
*not* registered, so the UNIX call fell through to real JDK bytecode
(`sun.nio.ch.ServerSocketChannelImpl` → `UnixDomainSockets.init()` →
`UnsatisfiedLinkError`) — and even with those natives implemented, the
resulting channel would have been an object CratonVM's poller cannot select
on.

### 2. `sc_read` held the `tcp_registry` read lock across a blocking `read()`

With UDS bind/accept/connect working, the test still hung *after* a successful
accept — the client's `SocketChannel.read()` never returned. Tracing
(`CRATONVM_DBG_SC_READ=1`) showed the endpoint accepted exactly one connection
and the acceptor thread then stopped dead, with the response never written.

`socket_channel::sc_read` resolved the socket like this:

```rust
let map = tcp_registry().read();
match map.get(&id) {
    Some(TcpHandle::Stream(s)) => try_read_nb(s, &mut buf),   // <- lock still held
```

For a **blocking**-mode channel the underlying `TcpStream` is left in OS
blocking mode, so `try_read_nb` parks indefinitely — with the registry read
lock held. The endpoint's acceptor thread then called `tcp_register(...)` for
the socket it had just accepted, which needs the **write** lock, and
`parking_lot`'s `RwLock` parks new readers behind a waiting writer, so the
whole registry seized up:

* main thread — blocked in `SocketChannel.read`, holding `tcp_registry` read
* acceptor thread — blocked in `tcp_register`, waiting for `tcp_registry` write

This is a **pre-existing, family-wide hazard**, not something the UDS work
introduced: `sc_write`, `sc_write_gathering` and `sc_read_scattering` had the
same shape. It stayed latent because it needs a client and a server that both
use `SocketChannel` inside one VM — which is exactly what this test does.
(`net.rs`'s `NetSocketHandle::Stream` had already been made `Arc<TcpStream>`
for precisely this reason; `socket_channel.rs` never got the same treatment.)

## Fix

**`native-io/src/uds.rs` (new)** — AF_UNIX stream sockets on the raw platform
socket API: `Ws2_32` FFI on Windows (10 1803+ ships `afunix.sys`; `WSAStartup`
is issued once so a UDS connector can be the first socket the VM opens),
`libc` on Unix. `std` has no portable AF_UNIX support and none at all on
Windows. Exposes `UdsListener` (bind/listen/accept, unlinking the socket file
on drop so a connector restart can re-bind the same path) and `connect()`.

Accepted/connected sockets are adopted into `std::net::TcpStream` via
`from_raw_socket`/`from_raw_fd`: every operation the channel layer performs on
a live connection (`recv`/`send`/`shutdown`/`FIONBIO`/handle duplication for
the selector) is protocol-agnostic, so the whole existing `TcpHandle::Stream`
path works unchanged. Only address decoding is not — `local_addr()` /
`peer_addr()` fail on a non-INET `sockaddr` — so UDS channels read their path
out of the synthetic channel state (`F_UDS_PATH`) instead.

**`native-io/src/socket_channel.rs`**
* `TcpHandle::UnixListener(UdsListener)`; `F_FAMILY` + `F_UDS_PATH` channel
  state slots.
* `{Server,}SocketChannel.open(ProtocolFamily)` registered (plus the
  `SelectorProviderImpl` / `WEPollSelectorProvider` / `EPollSelectorProvider`
  factory forms); `bind` / `connect` dispatch on a
  `java.net.UnixDomainSocketAddress` argument; `accept` on a UDS listener; and
  `getLocalAddress()` / `getRemoteAddress()` return a
  `UnixDomainSocketAddress`. `isBound()` treats "a listener id is registered"
  as bound for the UNIX family, which has no port — so
  `Connector.getLocalPort()` stays -1, exactly as on HotSpot.
* `TcpHandle::Stream` now holds `Arc<TcpStream>`, and the four I/O natives go
  through a new `resolve_stream(id)` that clones the handle out and **releases
  the registry lock before the syscall** — the root-cause-2 fix.

**`native-io/src/nio_selector.rs`** — `SelectableHandle::RawListener(i64)` so a
UDS listener can be polled by raw OS handle (it cannot be `try_clone`d into a
`std` type). Sound because `sc_close` calls `deregister_fd_everywhere(id)`
*before* dropping the registry entry that owns the socket.

**`native-builtins/src/phases_late/net_channels.rs`** — the three throwing
`UnixDomainSocketAddress` stubs deleted; the real class's own bytecode runs.

## Verification

* `org.apache.tomcat.util.net.TestXxxEndpoint` → `OK (3 tests)`, 4/4 runs
  (3× JIT on, 1× `--nojit`). Was `FAILURES!!! Tests run: 3, Failures: 1`.
* Standalone probes in [`tools/uds-probe`](../../../../tools/uds-probe) cover
  the bind → accept → read/write → close round trip (`UdsProbe.java`) and the
  Tomcat-shaped "blocking acceptor + Selector poller" topology
  (`UdsSelectorProbe.java`); both match HotSpot output line for line.
* `org.apache.tomcat.util.net.TestXxxEndpoint` → `OK (3 tests)` again after
  merging current `origin/dev` into the branch.
* `cargo test -p cratonvm-native-io` — the four new `uds::tests` pass,
  including a real socket round trip. (The one failure in that crate,
  `nio_selector::tests::t19_7_a_select_with_timeout_respects_deadline`, fails
  identically on unmodified `origin/dev` on this host and is unrelated.)
* **No throughput cost on the shared read/write path.** `TcpHandle::Stream`
  becoming an `Arc` and `resolve_stream` releasing the lock touch every NIO
  channel read and write, so they were A/B'd against a control binary built
  from the same `origin/dev` merge base, interleaving control/fix per round to
  cancel host-load drift, on the most socket-lifecycle-intensive class in the
  suite (`TestHttpServletDoHeadInvalidWrite0ValidWrite0` — 288 tests, one
  Tomcat start/stop each):

  | leg | mean | min | max |
  |-----|-----:|----:|----:|
  | control (`origin/dev`) | 259.3 s | 230.5 s | 298.0 s |
  | fix | 240.4 s | 216.2 s | 262.3 s |

  The fix was faster in 3/3 rounds — expected, since the registry lock is now
  held for the map lookup only rather than across the whole syscall.

  **Do not read a single run of this class as signal.** An early
  non-interleaved pair measured 246 s (control) vs 427 s (fix), which was pure
  noise. The whole `TestHttpServletDoHead*` family sits right at the suite
  runner's default 300 s timeout — the 646-class `fullsuite-local-20260728`
  reference has 49 of them PASSing at 287–296 s and 15 HANGing at 300 s — so
  any run of that family against a 300 s timeout flips PASS↔HANG on host load
  alone. Compare suite runs for this family at `-TimeoutSec 600` or higher.

### Full 646-class suite rerun

`run-tomcat-suite.ps1 -Category all -Parallel 6 -TimeoutSec 600`, fix binary,
run `uds-fix-leg-20260727`:

| | fix (600 s timeout) | reference `fullsuite-local-20260728` (300 s) |
|---|---:|---:|
| PASS | 564 | 547 |
| FAIL | 44 | 40 |
| HANG | 37 | 58 |
| NOSUMMARY | 1 | 1 |

`org.apache.tomcat.util.net.TestXxxEndpoint`: **FAIL → PASS**. 22 classes went
non-PASS → PASS (almost all `TestHttpServletDoHead*`, which the longer timeout
lets finish).

Five classes went PASS → non-PASS. Each was then re-run **interleaved** against
a control binary built from the same `origin/dev` merge base, and all five are
pre-existing flakes, not regressions:

| class | control | fix |
|---|---|---|
| `servlets.TestDefaultServletOptions` | FAIL (2/144) | PASS (144) |
| `servlets.TestWebdavServletOptionsFile` | FAIL (1/104) | FAIL (1/104) |
| `servlets.TestWebdavServletOptionsUnknown` | PASS (104) | PASS (104) |
| `el.TestELInJsp` | PASS, 280 s | PASS, 430 s (the suite HANG was the 600 s timeout under 6-way load) |
| `tribes.group.TestGroupChannelSenderConnections` | see below | see below |

`TestGroupChannelSenderConnections` needed 40 interleaved rounds per binary to
settle, because a single 8-round sample read 8/8 control vs 5/8 fix and looked
like a regression:

| | PASS | non-PASS (mostly `EXCEPTION_ACCESS_VIOLATION`) |
|---|---:|---:|
| control, JIT on | 25/40 | 15/40 |
| fix, JIT on | 23/40 | 17/40 |
| control, JIT off | 10/10 | 0 |
| fix, JIT off | 10/10 | 0 |

So it crashes at essentially the same rate on both binaries, only ever with the
JIT on, and its PASS in the 2026-07-28 reference was luck. The crash is a
JIT-frame SIGSEGV on a `Tribes-Task-Receiver` thread inside
`NioReplicationTask.drainChannel` — the pre-existing JIT stale-reference
family, unrelated to this fix and still open.

`CRATONVM_DBG_SC_READ=1` also enables a `[UDS] …` trace of
bind/accept/connect, kept as a permanent opt-in hook alongside the existing
`[SC_READ]` / `[SC_CLOSE]` diagnostics in the same file.
