# netty — investigate batch 04 of 13

**Status: TRIAGED 2026-08-12.** Every class explained; one CratonVM defect
fixed, four filed, three shown not to be CratonVM defects at all.

Part of a 184-class FAIL/HANG list split across 13 pages (see
[investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page
owns exactly the 15 classes below — do not touch classes listed in other batch
pages.

Originally found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite
run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`).
"status seen" is what that run recorded; "explained by" is the 2026-08-12
triage on the Azure Linux host (`20.80.105.49`), binary built from `origin/dev`
`8763197f2`, one class per VM (`--shards 1`).

## Outcome

| | |
| --- | --- |
| classes matching HotSpot | **7 PASS + 3 that fail on HotSpot too = 10 of 15** |
| CratonVM defects fixed | 1 (`DatagramChannel.localAddress()` bridge) |
| CratonVM defects filed | 4 |

**Three of the "failures" are not CratonVM's.** All three
`NativeImageHandlerMetadataTest` classes (`io.netty.channel`,
`io.netty.handler`, `io.netty.handler.codec`) fail **identically on stock
HotSpot JDK 25** — 1 found, 1 failed, same `collectAndCompareMetadata()`. They
compare generated GraalVM native-image metadata against a checked-in file and
are a build/fixture artefact of this runner. Nothing to fix.

Most of the rest is one root cause: **CratonVM fabricates NIO channels as
instances of the abstract `java.nio.channels.*` class while `channel.socket()`
returns the real `sun.nio.ch.*Adaptor`**, so the adaptor's own JDK bytecode
calls `*Impl`-declared methods that do not exist →
`NoSuchMethodError`/`AbstractMethodError`. See
[nio-channels-are-abstract-classed…](nio-channels-are-abstract-classed-so-jdk-adaptors-miss-impl-methods-20260812.md).

## Classes

Legend: ✅ matches HotSpot · ⚪ fails on HotSpot too · ❌ CratonVM defect

| class | status seen | explained by |
|---|---|---|
| `io.netty.channel.NativeImageHandlerMetadataTest` | FAIL | ⚪ **fails identically on HotSpot** (0/1) — native-image metadata fixture |
| `io.netty.channel.group.DefaultChannelGroupTest` | FAIL | ✅ **1/1** |
| `io.netty.channel.nio.NioEventLoopTest` | FAIL | ❌ 12/13 — `testChannelsRegistered` registers two `NioServerSocketChannel`s on one loop and asserts 2; CratonVM reports **1**. Selector registration accounting; not root-caused (its `Selector.open()` is `sun.nio.ch.SelectorImpl`, HotSpot's is `EPollSelectorImpl`) |
| `io.netty.channel.nio.NioIoHandlerEventCountTest` | FAIL | ✅ **3/3** |
| `io.netty.channel.oio.OioEventLoopTest` | FAIL | ✅ **3/3** |
| `io.netty.channel.pool.FixedChannelPoolMapDeadlockTest` | FAIL | ✅ **2/2** |
| `io.netty.channel.socket.nio.NioDatagramChannelTest` | FAIL | ❌ 1/4 → [abstract-classed NIO channels](nio-channels-are-abstract-classed-so-jdk-adaptors-miss-impl-methods-20260812.md). `localAddress()` fixed; the no-arg `openDatagramChannel()` and the `getOption`/`setOption`/`supportedOptions` surface must land together |
| `io.netty.channel.socket.nio.NioServerDomainSocketChannelTest` | FAIL | ❌ 6/7 — `testNioChannelOption`'s `SO_REUSEADDR` round-trip returns `0`; same generic `SocketOption` surface, UNIX-family server channel |
| `io.netty.channel.socket.nio.NioSocketChannelTest` | FAIL | ✅ **8/8** |
| `io.netty.channel.unix.NativeInetAddressTest` | FAIL | ❌ 1/2 → [Inet6Address drops the scope id](inet6address-drops-the-scope-id-20260812.md) — and drops a **non-zero** `%7` too, not just `%0` |
| `io.netty.handler.NativeImageHandlerMetadataTest` | FAIL | ⚪ **fails identically on HotSpot** |
| `io.netty.handler.codec.AdaptiveCumulatorTest` | FAIL | ✅ **77/77** |
| `io.netty.handler.codec.MessageAggregatorTest` | FAIL | ✅ **2/2** |
| `io.netty.handler.codec.NativeImageHandlerMetadataTest` | FAIL | ⚪ **fails identically on HotSpot** |
| `io.netty.handler.codec.compression.BrotliIntegrationTest` | HANG | ❌ genuine hang → [hangs outside the interpreter](brotli-integration-test-hangs-outside-the-interpreter-20260812.md). 11/11 in 9.4 s on HotSpot; on CratonVM no output after t+3 s and the stack-dump watchdog never fires |

## Filed from this page

* `docs/internal/fixed-suite-bugs/datagramchannel-localaddress-bridge-FIXED-20260812.md`
  — the fix, and why the descriptor differs from SocketChannel's.
* [`nio-channels-are-abstract-classed-so-jdk-adaptors-miss-impl-methods-20260812.md`](nio-channels-are-abstract-classed-so-jdk-adaptors-miss-impl-methods-20260812.md)
  — the umbrella. Includes the four-line `openDatagramChannel()` patch that was
  written, measured and **backed out**, with the measurement showing why it
  cannot land without the option surface.
* [`inet6address-drops-the-scope-id-20260812.md`](inet6address-drops-the-scope-id-20260812.md)
* [`brotli-integration-test-hangs-outside-the-interpreter-20260812.md`](brotli-integration-test-hangs-outside-the-interpreter-20260812.md)

## Repro

These bind real sockets — run **one class per VM** (`--shards 1`), or parallel
forks collide on ports and unix-socket paths and you get failures that are not
the VM's.

```bash
cd apps/netty-suite-runner
printf 'io.netty.channel.socket.nio.NioDatagramChannelTest\n' > /tmp/one.txt
bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 600 \
  --bin <cratonvm> --out /tmp/repro

# HotSpot oracle, same classpath
CP=$(sed -n 2p common.args)
/data/toolchain/jdk-25/bin/java -cp "$CP" -Duser.timezone=UTC \
  -Djunit.jupiter.execution.timeout.default=120s -Dcraton.batch=1 \
  CratonRunner io.netty.channel.socket.nio.NioDatagramChannelTest
```
