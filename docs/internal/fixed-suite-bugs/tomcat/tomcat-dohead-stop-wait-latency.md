# TARGETED FIX LANDED: Tomcat DoHead stop wait latency

**Status:** The identified close/accept stop-wait source was fixed on
2026-07-01. Full Tomcat per-subtest timing still needs an external
`../../../../apps/tomcat-suite-runner` checkout; that runner is not present in this worktree.

Full investigation + profiling: memory note
`reference_tomcat_dohead_gc_safepoint_deadlock`.

## Problem

The Tomcat `TestHttpServlet` / `TestHttpServletDoHead*` family runs about 156
sub-tests, each a full Tomcat start + HTTP exchange + stop. Historical profiling
showed about 2.86s/sub-test, or about 450s/class, too slow to finish within the
suite's 600s timeout under `-Parallel 4`.

## Key Finding

The iteration was wait-bound, not CPU-bound. Prior samply/ETW profiling showed
only about 1.5 cores used; the test thread was about 40% on-CPU and 60% waiting.
The CPU side was already addressed by the `LinkResolver` cache cap increase
(`79dfe23c`). Remaining wall-time came from stop-side waits: Tomcat thread joins,
acceptor/poller shutdown, executor termination, and HTTP round trips.

## Fix: close-aware blocking accept

Two accept paths could retain a listener handle while another thread closed the
Java socket/channel:

- `../../../../native-io/src/socket_channel.rs`: `ServerSocketChannel.accept()` cloned the
  `TcpListener` and could block in OS `accept()` on that duplicate. Closing the
  channel removed the registry entry, but the duplicate could remain blocked.
- `../../../../native-io/src/net.rs`: `sun.nio.ch.Net.accept()` held an `Arc<TcpListener>`
  across blocking accept. Closing the fd marked the registry entry closed, but
  the accept thread's `Arc` kept the OS listener alive until accept returned.

Both paths now use a close-aware nonblocking poll loop while inside the existing
GC blocked region. `close()`/registry removal is observed within a 10ms poll
interval, so stop can wake acceptors promptly instead of waiting for a stray
connection or OS-specific listener-close behavior.

The accepted stream is restored to the Java-visible blocking mode after accept,
so this does not change socket semantics for callers.

## Validation

Targeted regression tests:

```powershell
cargo test -p cratonvm-native-io blocking_accept_observes_channel_close_promptly
cargo test -p cratonvm-native-io t19_5_accept_observes_close_promptly
cargo test -p cratonvm-native-io
```

All 320 `cratonvm-native-io` tests pass.

Historical external measure:

```powershell
.\handle-monitor.ps1
.\run-tomcat-suite.ps1 -Start 28 -Count 1 -TimeoutSec 700
```

with `CRATONVM_REAL_NET_SOCKETS=1`, `CRATONVM_REAL_AQS=1`, and
`CRATONVM_ROOTSNAP_CACHE=1`.

As of 2026-07-01, `../../../../apps/tomcat-suite-runner` is not present in this checkout, so
the full class runtime and the <2s/sub-test target could not be remeasured here.
