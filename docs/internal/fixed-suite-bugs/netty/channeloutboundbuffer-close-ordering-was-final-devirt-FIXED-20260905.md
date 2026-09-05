# The netty close-ordering invariant was not an ordering bug — the JIT devirtualised two `final` JDK methods past the natives that shadow them

**Status:** ✅ RESOLVED (2026-09-05). Retired from
`docs/known-issues/netty/channeloutboundbuffer-close-ordering-three-classes-20260905.md`,
filed 2026-09-04.

`io.netty.resolver.dns.DnsNameResolverTest` logged
`IllegalStateException: close() must be invoked after the channel is closed.`
**384 times** on the tip of `dev` and **0 times** after the fix. HotSpot logs it
0 times; the same binary under `--nojit` logged it 0 times before the fix, which
is the whole finding in one line.

| class | before | after |
|---|---|---|
| `io.netty.resolver.dns.DnsNameResolverTest` | 384 occurrences, `ok=224 failed=0 aborted=8` | **0 occurrences**, `ok=224 failed=0 aborted=8` |
| `io.netty.handler.proxy.ProxyHandlerTest` | 0 on this host (`ok=47 failed=0`) | 0, `ok=47 failed=0` |
| `io.netty.handler.codec.http2.DataCompressionHttp2Test` | 0 on this host, `ok=40 failed=2` | 0, `ok=40 failed=2` — the 2 are **not** this bug, see §5 |

---

## 1. What the page said, and what was actually true

The page's title was "Three unrelated classes hit ChannelOutboundBuffer's
close() invariant" and its open question was *"Whether this is a genuine
ordering bug (state update happens too late) or a cross-thread visibility gap"*.
It had checked `sc_close` (`native-io/src/socket_channel.rs`) end to end and
ruled it out, correctly: that native really does clear its state synchronously
before returning.

Neither of the two options was right, and the ruled-out native was not
exonerated by being synchronous — **the state it cleared was not the state the
reader read.**

`java.nio.channels.spi.AbstractInterruptibleChannel` declares both of the
methods in play `public final`:

```java
public final boolean isOpen() { return !closed; }
public final void close() throws IOException {
    synchronized (closeLock) { if (closed) return; closed = true; }
    implCloseChannel();
}
```

CratonVM answers both from registered natives keyed on the channel classes
(`java/nio/channels/{Socket,ServerSocket,Datagram}Channel` and their
`sun.nio.ch.*Impl` twins), and `invoke_or_native` finds them by walking the
RECEIVER's superclass chain. The JIT's `final`-method devirtualiser
(`invoke::invokevirtual_site_final_owner`, `CRATONVM_JIT_FINAL_DEVIRT`,
default-on since 2026-08-28) rewrites such a site into a direct bind on the
DECLARING class — and the declaring class here is `AbstractInterruptibleChannel`,
which carries no native. So from the tier-up call onward, compiled code ran the
JDK classfile body and the interpreter ran the native, for the same call.

`final` promises that no subclass declares another BODY. It promises nothing
about CratonVM's native registry, and nothing in the door asked.

## 2. The two halves, measured

Both probes are netty's own shape — a small delegating method, called often
enough to be compiled. `AbstractNioChannel.isOpen()` is `return ch.isOpen()` on
a `SelectableChannel`-typed field; `NioDatagramChannel.doClose()` is
`javaChannel().close()`.

### 2a. `isOpen()` — `probes/ChannelStateAfterCloseCensus.java`

`isOpen()` is `return !closed`, and CratonVM's channel closes cleared their own
`chan_fields` side table without ever writing that field. A compiled caller read
the raw field and answered OPEN on a closed channel:

| row | before | after |
|---|---|---|
| `SocketChannel.isOpen` after close | **397,739 / 400,000 wrong**, first at call 2,261 | 0 / 400,000 |
| `ServerSocketChannel.isOpen` after close | **399,999 / 400,000**, first at call 1 | 0 / 400,000 |
| `DatagramChannel.isOpen` after close | **399,999 / 400,000**, first at call 1 | 0 / 400,000 |

The census also covers `Pipe.{Source,Sink}Channel`, `FileChannel`, `Selector`,
`SelectionKey.isValid`, `Socket.isClosed`/`isConnected` and
`ServerSocket.isClosed` — all clean before and after, and a live-channel row
that must keep reading OPEN so a screen that simply answered "closed"
everywhere could not pass. `Pipe` was clean because `pipe.rs` had already
learned this exact lesson (`G46-1`, "the `ServerSocket.bound` shape"); the three
socket families were missed.

The interface-typed spelling (`askChannel(Channel)`, an `invokeinterface`) was
clean throughout, which is what first separated "the JIT" from "the natives":
only the `invokevirtual`-on-a-`final`-method sites diverged.

### 2b. `close()` — `probes/ChannelCloseDevirtProbe.java`

`close()` opens with `synchronized (closeLock)`, and `native_dc_open` never ran
the JDK constructor that assigns it — `socket_channel.rs::init_channel_locks`
seeds it for the two stream channels and the datagram factory had no equivalent.
So the devirtualised body threw:

| arm | before | after |
|---|---|---|
| `DatagramChannel.close()` through the delegating method | **3,487 / 4,000 threw `NullPointerException`**, first at call 512, each leaving the socket open (and leaking the fd) | 0 / 4,000 |
| `SocketChannel.close()`, same shape | 0 / 4,000 | 0 / 4,000 |
| HotSpot, either | 0 / 20,000 | 0 / 20,000 |

The `SocketChannel` row is 0 in both arms because that family's `closeLock` was
already seeded. That asymmetry is what made the defect look like three unrelated
classes rather than one mechanism.

## 3. How netty turns that into the reported exception

`AbstractChannel$AbstractUnsafe.close()` runs `doClose0(promise)` and then, in
the same method's `finally`, `outboundBuffer.close(closeCause)`:

1. `doClose()` → `javaChannel().close()` → the devirtualised JDK body → `NPE`.
2. `doClose0` catches it and fails the close promise with the NPE. The channel
   was never closed.
3. The `finally` calls `ChannelOutboundBuffer.close(cause)`, whose
   `!allowChannelOpen && channel.isOpen()` guard is now true → the
   `IllegalStateException` the page is named after.
4. netty tries to fail the promise with THAT, finds it already failed with the
   NPE, and logs
   `Failed to mark a promise as failure because it has failed already: …
   (failure: java.lang.NullPointerException), unnotified cause: <the ISE>`.

That WARN line is in every one of the 384 DNS occurrences and is what finally
named the NPE. The page never saw it because it was reading the exception, not
the line above it.

## 4. The fix — three changes, smallest last

1. **`invoke::final_devirt_native_shadow`** (`vm/src/runtime/interpreter/invoke.rs`)
   screens the devirtualisation door against the native registry: refuse when a
   native for this `(name, descriptor)` is registered on the constant-pool
   class, the declaring class, or a **loaded subclass of the declaring class**.
   The third clause is the one that matters and it is the same rule
   `resolve_inline_site_from` learned for guarded inlines on 2026-09-04
   (`native-shadow-on-receiver-chain`) — asked the only way a site with no
   receiver can ask it, so `NativeMethodRegistry::owner_classes_for_method`
   inverts the registry index to answer it. Default ON; kill switch
   `CRATONVM_JIT_FINAL_DEVIRT_NATIVE_SCREEN=0`; counter
   `cratonvm_jit::FINAL_DEVIRT_NATIVE_SHADOW_REFUSED`.
2. **`socket_channel::mark_jdk_channel_closed`** writes the JDK's own `closed`
   flag from `sc_close`/`ssc_close`/`native_dc_close`, so the two views of a
   closed channel agree whoever asks — including reflection, `end(boolean)`'s
   `AsynchronousCloseException` check, and `close()`'s specified idempotence.
3. **`native_dc_open` seeds `closeLock`/`keyLock`/`regLock`/`interruptor`** the
   way `socket_channel.rs` does, and the datagram family registers
   `implCloseChannel`/`implCloseSelectableChannel`.

### Two fixes, each independently sufficient — measured, not assumed

(1) is a JIT-dispatch fix and (2)+(3) are a channel-state fix, and they cover
the same faces from different levels. To find out whether either was
decoration, the `native-io` half was reverted to `dev` and rebuilt, giving one
binary per arm on the same tip:

| probe / face | screen ON | screen OFF |
|---|---:|---:|
| `ChannelCloseDevirtProbe`, datagram close, **without** (2)+(3) | 0 / 4,000 | **3,487 / 4,000**, first at call 512 |
| `ChannelStateAfterCloseCensus`, `DatagramChannel.isOpen`, **without** (2)+(3) | 0 / 400,000 | **399,999 / 400,000** |
| both, **with** (2)+(3) — the shipped state | 0 | 0 |

So the screen alone fixes it, and the state fix alone fixes it. Shipping both
is deliberate: the screen stops compiled code reaching the JDK body at all and
generalises past channels; the state fix makes that body CORRECT for anything
else that reaches it (reflection, `end(boolean)`'s `AsynchronousCloseException`
check, `close()`'s specified idempotence).

**A note on how this table was wrong once.** An earlier revision of this page,
and of the two code comments, said the seeding was *not* sufficient — that a
devirtualised `close()` got one step further and died on `"this.stateLock" is
null` inside `DatagramChannelImpl.implCloseSelectableChannel`. That was
measured and true on the tree this branch started from. A same-day `dev` merge
made the `implCloseChannel`/`implCloseSelectableChannel` registrations win that
dispatch, and the claim went stale with nothing in this branch changing. It was
caught by re-running the B arm after the merge rather than by review, which is
the argument for re-measuring a kill switch's stated symptom every time the
base moves — a B arm that no longer produces the symptom it documents reads as
"the switch does nothing".

**What the switch DOES do is a separate instrument**, and it is the one to
trust when both halves are in: `CRATONVM_DBG_JITC=1` on the regression fixture
prints exactly three refusals, and they are precisely the two methods this page
is about —

```text
final-devirt REFUSED java/nio/channels/SelectableChannel.isOpen()Z  (declared on …AbstractInterruptibleChannel)   x2
final-devirt REFUSED java/nio/channels/SelectableChannel.close()V   (declared on …AbstractInterruptibleChannel)   x1
```

with the count also available as `FINAL_DEVIRT_NATIVE_SHADOW_REFUSED`.

### The one case the screen cannot see

A native-carrying subclass that is not loaded yet when the site compiles. It is
skipped deliberately — an unloaded class cannot be a receiver, and treating
every unloaded owner as a hazard would refuse every `close()V` / `equals` /
`hashCode` site in the tree, since those names are registered on dozens of
classes most runs never load. It is the same residual the guarded-inline screen
carries (that one keys on the receiver it has SEEN), and it is stated in the
code rather than left implicit.

## 5. What is NOT fixed, and was never this bug

`DataCompressionHttp2Test` reports `ok=40 failed=2` before and after. Both
failures are `encodingTooBigMessage[snappy]`, and both are
`assertTrue(serverLatch.await(5, SECONDS))` — a five-second deadline inside the
test, on a class that takes **159.8 s on CratonVM against 14.6 s on HotSpot**.
Snappy itself is correct here: `SnappyTest` 13/13, `SnappyDecompressorTest`
31/31, `SnappyFrameDecoderTest` 29/29. This is the compression-throughput wall
already filed as
`docs/known-issues/netty/compression-cluster-testhugedecompress-180s-throughput-wall-20260827.md`,
reached through a different test's own timeout. It is recorded here only so the
next reader does not re-open this page for it.

## 6. Reproducing

```bash
# the two probes, on any binary
cratonvm --java-home <jdk25> -cp probes ChannelStateAfterCloseCensus
cratonvm --java-home <jdk25> -cp probes ChannelCloseDevirtProbe
# and the B arm, same binary
CRATONVM_JIT_FINAL_DEVIRT_NATIVE_SCREEN=0 cratonvm ... ChannelCloseDevirtProbe
```

The regression test is `vm/tests/jit_final_devirt_native_shadow.rs`, which runs
its fixture in BOTH the default and `--nojit` arms — because the defect is that
the two disagreed, and a test that ran only one of them could not see it.
