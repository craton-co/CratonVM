# `sun.nio.ch.SocketChannelImpl.stateLock` reads null — a connection-killing NPE across 4 unrelated Tomcat test classes

## Status

✅ **FIXED 2026-09-05**, branch `fix/tomcat-statelock-and-atomicref-20260905`.
Diagnosed, reproduced in a 20-line probe, fixed and A/B-verified on all four
affected classes in one session.

**Severity as filed:** MEDIUM-HIGH — killed in-flight HTTP/1.1 and HTTP/2
connections outright (not merely a slow path), reproducibly, across independent
test areas (HTTP/2 flow control, servlet async I/O).

## The measurement

Windows box, `apps/tomcat` fixture, `-XX:+UseZGC`, one process per class. Same
binary in both arms except for the fix. `stateLock` counts occurrences of the
NPE in the class's log.

| class | before | after |
|---|---|---|
| `org.apache.coyote.http2.TestFlowControl` | `FAILURES!!!` 2 of 2, stateLock=2 | **`OK (2 tests)`**, stateLock=0 |
| `org.apache.coyote.http2.TestHttp2Section_5_1` | `FAILURES!!!` 2 of 26, stateLock=2 | **`OK (26 tests)`**, stateLock=0 |
| `org.apache.coyote.http2.TestHttp2Section_6_1` | `FAILURES!!!` 4 of 14, stateLock=8 | **`OK (14 tests)`**, stateLock=0 |
| `org.apache.catalina.core.TestAsyncContextImpl` | `FAILURES!!!` 4 of 70, stateLock=8 | **`OK (70 tests)`**, stateLock=0 |

Those failing counts are the Azure `zgc-3gc-20260905/shard-0` counts to the
test, on a different OS — which is the first thing that said this is a
structural defect and not the host contention the surrounding census entries
are full of.

## Root cause

`sun.nio.ch.SocketChannelImpl` declares

```java
private final Object stateLock = new Object();
```

and every method on it that reports or changes state runs
`synchronized (stateLock)`. `ServerSocketChannelImpl` declares its own.

CratonVM does not run those constructors: `native-io/src/socket_channel.rs`'s
factories mint the channel with `alloc_channel_as_impl`, which allocates an
instance of the concrete `sun.nio.ch.*Impl` class at its full real field width
and then seeds the fields the VM knows the real bytecode needs. That seeding —
`init_channel_locks` — covered exactly three fields:

```rust
for f in ["closeLock", "keyLock", "regLock"] { … }
```

All three are INHERITED (`AbstractInterruptibleChannel`,
`AbstractSelectableChannel`). `stateLock` is declared on the CONCRETE class, was
not in the list, and read null. So `SocketChannelImpl.toString()` executed
`monitorenter null`:

```
java.lang.NullPointerException: Cannot enter synchronized block because "this.stateLock" is null
	at sun.nio.ch.SocketChannelImpl.toString(SocketChannelImpl.java:1583)
	at org.apache.tomcat.util.net.NioChannel.toString(NioChannel.java:241)
	at org.apache.tomcat.util.net.SocketWrapperBase.toString(SocketWrapperBase.java:546)
	at org.apache.coyote.AbstractProcessorLight.process(AbstractProcessorLight.java:81)
```

This is the same failure class as the already-fixed
`../CRATONVM_BUGS/BUG-DF01-nio-keyfor-keylock-null-selector-loop.md` (`keyLock`
on the abstract superclass, killing the NioEndpoint poller) — a synthetic/native-constructed
real-JDK NIO channel whose `private final` lock object the real bytecode depends
on was never populated. DF01's fix did not cover this field, and could not:
`stateLock` is one class further down.

## Why a `toString()` was killing connections

Every site Tomcat reaches `toString()` through is an ERROR or TEARDOWN path that
does not expect it to throw, so the NPE propagated out and aborted the request
in flight:

* `AbstractProcessorLight.process` building a diagnostic string,
* `NioEndpoint$NioSocketWrapper.doClose` via `SocketWrapperBase.close`,
* `StringManager.getString` formatting the socket into a `SEVERE`/`FINE`
  message from `AbstractProtocol$ConnectionHandler.process`.

`TestFlowControl`'s log states the causality itself — "An error occurred during
processing that was fatal to the connection", with this NPE as the cause — and
the client sees the response truncated: `IOException: End of input stream with
[9] bytes left to read`, `connection closed before response head`,
`SocketException: Broken pipe` (Linux) / `Connection aborted` (Windows).

## The 20-line reproducer

No Tomcat needed. On the pre-fix binary this throws on **line 8**:

```java
ServerSocketChannel ssc = ServerSocketChannel.open();
System.out.println("ssc(unbound) = " + ssc);   // NPE: this.stateLock is null
```

`regression-suite/src/RChannelToString.java` is that probe, promoted to a
differential vector: it renders a listener and a connected pair in every state
(unbound / bound / fresh / connected / non-blocking / closed) with ports masked,
and the suite diffs the strings against HotSpot's. A VM that throws prints
nothing and fails; a VM that answers a different string fails the diff.

## The fix

Three parts, all in `native-io/src/socket_channel.rs`.

1. **`init_channel_locks` seeds `stateLock`** alongside the other three. The
   seed is the channel object itself (the existing convention there — the field
   only has to be a non-null stable monitor, and using the channel avoids an
   allocation that a moving GC could relocate the receiver across).
   `set_field_by_name` is a no-op when the slot is absent, so naming a field
   only some channel classes declare costs nothing on the others.

2. **Native `toString()` on `sun/nio/ch/SocketChannelImpl` and
   `sun/nio/ch/ServerSocketChannelImpl`**, answering from the identity-keyed
   `chan_fields` side table that actually holds this channel's state. This is
   DF01's remedy applied to this field: the bytecode that dereferences the
   un-populated slot never runs. It also fixes what the seed alone would
   leave — `state`, `isInputClosed`, `isOutputClosed` and `localAddress` are
   real object slots CratonVM never writes, so the seeded bytecode would render
   every channel as a permanently `unconnected` one with no addresses.

   Registered on the CONCRETE spellings ONLY. `toString()` is a method every
   object has, and native dispatch walks the receiver's superclass chain, so a
   registration on the abstract `java.nio.channels.SocketChannel` would take
   `Object.toString()` away from every third-party subclass of it (the
   barchart-udt shape recorded on `foreign_nio_receiver`). The `sun.nio.ch`
   classes are package-private in `java.base` and cannot be subclassed from
   outside it, and each native additionally keeps the `foreign_nio_delegate`
   guard.

3. **Native `implConfigureBlocking(boolean)`** on the same two classes — see
   the Tribes residual below.

**What was tried and reverted.** A `force_native_over_real_jdk_bytecode`
(`check_override`) entry for the two new methods. MEASURED against the binary
that does NOT contain it: the natives already win, and `ChanToString` prints
CratonVM's own rendering in every state. An unnecessary entry in that list is
not inert — it forces a native ahead of bytecode for every future caller — so it
came back out.

**One deliberate faithfulness detail.** A first cut of `ssc_to_string` printed
the local address for a bound TCP listener, which is more useful and WRONG.
`javap -c sun.nio.ch.ServerSocketChannelImpl` on JDK 25.0.3.9 reads
`getfield localAddress; ifnull → "unbound"; invokevirtual isUnixSocket; ifeq →
end; append(addr)` — there is no arm appending an INET address, and HotSpot
prints `sun.nio.ch.ServerSocketChannelImpl[]` with nothing between the brackets.
This method exists to remove a divergence from HotSpot, not to add one.

## Residuals from the original page, resolved

**`TestHttp2Section_8_2` — NOT this cause.** Its Azure ZGC log contains **zero**
occurrences of `stateLock`, and its results row is
`org.apache.coyote.http2.TestHttp2Section_8_2,124,300,HANG,21.27` — `rc=124` is
the harness cap, 300 s is the cap exactly, and the load average as it finished
was 21.27. A 6658-test class capped on a loaded host is the shape
`nonpassed-class-census.md`'s own loadavg column exists to identify. It stays
where it is, unattributed to this bug.

**The Tribes `implConfigureBlocking` NPE — the same family, one field over.**
`zgc-3gc-20260905/shard-0/…TestDataIntegrity.log` carries

```
Caused by: java.lang.NullPointerException
	at sun.nio.ch.SocketChannelImpl.implConfigureBlocking(SocketChannelImpl.java:706)
	at java.nio.channels.spi.AbstractSelectableChannel.configureBlocking(AbstractSelectableChannel.java:328)
	at org.apache.catalina.tribes.transport.nio.NioSender.configureSocket(NioSender.java:185)
	at org.apache.catalina.tribes.transport.nio.NioSender.connect(NioSender.java:316)
```

`NioSender.connect` does `socketChannel = SocketChannel.open()`, so the receiver
is one of ours. `SocketChannelImpl.implConfigureBlocking`'s body is
`readLock.lock(); … synchronized (stateLock) …` — `readLock` is a
`private final ReentrantLock` from the same never-run constructor, one field
over from `stateLock`. Registering `implConfigureBlocking` natively means the
real bytecode does not run whichever way the call arrives, so neither field is
dereferenced. (This is the residual the original page left as "not confirmed to
share this root cause"; it does share the family, and it is a different field
from the one in the title.)

**The construction path, pinned.** The original page could not name it. It is
`alloc_channel_as_impl` (which mints `sun/nio/ch/SocketChannelImpl` at the real
field width, so the slot genuinely exists) plus `init_channel_locks` (which
seeded three fields and not this one). Both are in
`native-io/src/socket_channel.rs`; no separate synthetic constructor is
involved.

## Regression coverage

`regression-suite/src/RChannelToString.java`, registered in `run.sh`'s
`CORE_CLASSES`. Asserts the rendering of both channel classes in six states
against HotSpot, and exercises `configureBlocking(false)` between two of them so
the `implConfigureBlocking` half is on the path.

## Related

* `../CRATONVM_BUGS/BUG-DF01-nio-keyfor-keylock-null-selector-loop.md` — the
  fixed sibling (`keyLock` on the abstract superclass, not `stateLock` on the
  concrete one).
* `docs/known-issues/tomcat/nonpassed-class-census.md` — its "HTTP/2 — 4 … Not
  diagnosed" cluster and its `TestAsyncContextImpl` row, updated by this fix.
* `docs/known-issues/tomcat/tribes-multicast-family-still-environmental.md` —
  that family's other failures remain environmental; only the
  `implConfigureBlocking` NPE above is claimed here.
