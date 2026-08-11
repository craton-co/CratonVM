# A blocked reader never woke up — `close()` cannot reach a thread parked in `recv`

**Status:** FIXED on the two real socket surfaces (code landed 2026-08-07;
**verified 2026-08-11**, see "Measured 2026-08-11" below — the default-mode arm
now wakes and throws `SocketException`). The third reader, the *synthetic*
`java.net.Socket` surface, was **still live on 2026-08-11** and is fixed by the
code landed that day in `native-builtins/src/net_phase_e.rs` — unverified, no
binary was built in that session either. Lane W2-2 of the jdk-wave2 pool.

## The failure

Two classes, one assertion string, both failing in **both** `--real-jdk` and
`--jdk-only`; HotSpot 25 passes both (exit 0).

```
RJdkNio:  AssertionError: the blocked reader never woke up
            at RJdkNio.selectorAndAsyncClose(RJdkNio.java:363)
RJdkNet:  AssertionError: the blocked reader never woke up
            at RJdkNet.soTimeoutAndAsyncClose(RJdkNet.java:248)
```

Both are *asynchronous close* — not interrupt, not `SO_TIMEOUT`. A reader
thread parks in a read, the main thread closes the socket 200 ms later, and the
reader must come back with an exception:

| site | reader parks in | closed by | expected |
| --- | --- | --- | --- |
| `RJdkNio:347` | `SocketChannel.read(ByteBuffer)` | `SocketChannel.close()` | `AsynchronousCloseException` |
| `RJdkNet:234` | `Socket.getInputStream().read()` | `Socket.close()` | `SocketException` |

Neither test touches `Thread.interrupt()`, and `RJdkNet`'s `setSoTimeout`
block is a *different, already-passing* socket earlier in the same method.

Both are late assertions. Wave 1 fixed the earlier failures in these two files
(`Files.copy` at RJdkNio:102, `SO_RCVBUF` at RJdkNet:180), so these paths had
almost certainly never executed on this VM before.

## One defect, two implementations

(Three, as it turned out — the synthetic `java.net.Socket` surface is a third
independent registry with the same shape, and it is on neither *failing test's*
path, which is why it reads as a residual rather than a symptom. See "The third
reader" below.)

They are the **same defect shape in two independent code paths**, so one fix
does not cover both. The two Java surfaces do not share a socket registry:

```
Socket.getInputStream().read()          SocketChannel.read(ByteBuffer)
  -> NioSocketImpl.implRead                -> native-io/socket_channel.rs::sc_read
  -> SocketDispatcher.read0                   (registered on
  -> native-io/net.rs::net_read0               java/nio/channels/SocketChannel
     registry: net::net_sockets()              and sun/nio/ch/SocketChannelImpl)
     Arc<TcpStream>                          registry: socket_channel::tcp_registry()
                                                        Arc<TcpStream>
```

`java/net/Socket` natives are dropped wholesale under the default
`CRATONVM_REAL_NET_SOCKETS` (`native-api/src/registry.rs:5417`), so real JDK
bytecode runs and lands on `net.rs`. `java/nio/channels/SocketChannel` is **not**
in that drop list, so `sc_read` services the channel directly and
`NioSocketImpl` is never involved. Both registration blocks are
`NativeKind::Bridge`, which is why `--jdk-only`'s `drop_synthetic_stubs` does
not change the outcome — it is an ordinary Compatible-mode defect that both
modes inherit.

## Root cause (shared)

Both readers park inside a genuine OS `recv` on an `Arc<TcpStream>` they cloned
out of the registry. Neither `close()` path can end that park:

* `net.rs::close_net_fd` marks the slot `Closed` and issues
  `shutdown(Shutdown::Both)`. It does **not** close the OS handle — it cannot,
  because the map's `Arc` is not the last one; the reader holds a clone.
* `socket_channel.rs::sc_close` is worse: `lingering_channel_close` issues
  `shutdown(Shutdown::Write)` only (deliberately — see its comment on RST-prone
  `Shutdown::Both`), then `tcp_remove` drops the map's `Arc`, which again closes
  nothing.

`shutdown` is not a substitute for a close here, and the difference is what made
this look like two separate bugs:

* On **Linux**, `shutdown(SHUT_RD)` does wake a parked `recv` with EOF. So
  `net.rs` (which shuts down *Both*) happens to work there, while
  `socket_channel.rs` (write side only) fails on every platform.
* On **Windows**, no `shutdown` aborts a pending blocking call — only
  `closesocket` does. So both fail.

HotSpot breaks the same park by taking the descriptor away underneath it:
`closesocket()` on Windows, or `dup2` of a pre-closed descriptor plus a signal
to the reader thread on Unix (`NativeDispatcher.preClose`). Neither is
expressible over an `Arc<TcpStream>` without closing a handle another thread is
mid-syscall on, which is a use-after-close the instant the OS recycles the
number.

`eintr.rs` is **not** implicated. The EINTR retry loops are correct; no signal
is delivered here, so nothing spins. Nor is a lock held across the blocking
call: both read paths already clone the `Arc` out under a brief read guard and
drop the map lock before the syscall (AUDIT 2026-05-17), and both `close`
paths were reached and returned — `RJdkNio`'s assertion is at line 363, *after*
`client.close()` at 362, so the closing thread was never blocked.

## The fix — park in `poll`, not in `recv`

The repo already had this shape on the accept side:
`net.rs::net_accept_close_aware` and `socket_channel.rs::accept_close_aware`
put the listener in non-blocking mode and re-ask the registry every 10 ms, so a
parked accept observes a close. `socket_channel.rs::sc_blocking_read` gets the
same property for free by re-`resolve_stream`ing on every pass. The blocking
*read* paths were the two that did not.

A sleep-poll loop is wrong for reads — it would add up to 10 ms of latency to
every HTTP request — so these park in `poll`/`WSAPoll` with a bounded timeout
instead. The poll returns the instant the socket is readable, so payload is not
delayed; the timeout only bounds how long a reader stays parked after a close.

### `native-io/src/net.rs`

* `poll_stream_readable(stream, timeout_ms) -> Option<io::Result<bool>>` —
  `pub(crate)` wrapper over the existing `net_poll_stream`/`net_poll_raw`
  primitives (WSAPoll on Windows, `poll(2)` on Unix, EINTR already reported as
  "not ready"). `None` means the target has no poll primitive at all, and is the
  signal to fall back to a plain blocking read rather than spin on a stub.
* `read_retry_eintr` — the EINTR retry lifted verbatim out of `net_read0`.
* `net_read_close_aware(fd, stream, buf)` — poll, then re-check
  `net_stream_still_registered(fd)`, then read. Returns
  `ErrorKind::Interrupted("socket closed")` once the registry slot is no longer
  a `Stream`. The registry read guard is taken per pass and never held across
  the poll.
* `net_read0` uses it **only on the `can_park` branch** (`!net_fd_is_nonblocking(fd)`).
  The non-blocking branch — every `Socket.setSoTimeout` reader, i.e. the
  `NioSocketImpl.timedRead` + `Net.poll` protocol — is byte-for-byte unchanged,
  and `WouldBlock` is still propagated so `net_read0` can answer the JDK's
  `IOStatus.UNAVAILABLE` (-2) sentinel.

`net_err` has no `Interrupted` arm, so the new error renders through its default
`SocketException: …`. It does not actually matter which text arrives:
`NioSocketImpl.endRead` runs in the `finally` of `implRead`, sees
`state >= ST_CLOSING`, and throws `SocketException("Socket closed")` over the
top of it — which is what HotSpot produces and what `RJdkNet:249` asserts.

### `native-io/src/socket_channel.rs`

* `read_close_aware(id, stream, buf)` — the same loop against
  `stream_still_registered(id)`, returning `ErrorKind::Interrupted`.
* `sc_read` and `sc_read_scattering` use it when `read_blocking_flag` is true.
  **A non-blocking channel takes the old path unchanged**, which is every
  selector-driven reactor read in Netty/Tomcat/Jetty — the blast radius is
  limited to channels that are genuinely in blocking mode.
* `channel_exception(ctx, simple_name)` builds the real
  `java/nio/channels/<name>` through `new_object` + no-arg `<init>`, in the
  style of `lib.rs::native_file_lock_impl_release`. `closed_or_io_error` maps
  `ErrorKind::Interrupted` to `AsynchronousCloseException` and everything else
  through the existing `map_err`.
  The concrete class is load-bearing: `RJdkNio` catches
  `AsynchronousCloseException`, then `ClosedChannelException`, then
  `IOException`, and records which arm ran. A message-prefix `IOException`
  lands in the wrong arm.
* Second half of the same assertion block, `RJdkNio:370-374`: a read on an
  already-closed channel must throw `ClosedChannelException`. `sc_close` calls
  `cf_clear`, so `read_reg_id` answers `None` and `sc_read` used to raise
  `IOException("read: channel not connected")` — which that `catch` walks past.
  It now answers `ClosedChannelException` when `F_OPEN` reads back 0, and keeps
  the old IOException for an open-but-unconnected channel.

## Tests added

`native-io/src/net.rs::tests`

* `close_wakes_a_reader_parked_on_the_fd_being_closed` — the half the
  pre-existing `t19_5_close_unblocks_blocking_stream_read` does **not** cover.
  That test registers the *client* and asserts the *server's* read wakes, i.e.
  it only ever proved the peer observes the shutdown. It passes on an unfixed
  tree.
* `close_aware_read_still_delivers_bytes_promptly` — the close-aware read is
  still a read.

`native-io/src/socket_channel.rs::tests`

* `a_close_breaks_a_parked_blocking_channel_read`
* `a_close_aware_read_still_delivers_bytes`

## Verifying once a binary exists

```
cratonvm.exe --real-jdk -cp regression-suite/classes RJdkNio
cratonvm.exe --jdk-only -cp regression-suite/classes RJdkNio
cratonvm.exe --real-jdk -cp regression-suite/classes RJdkNet
cratonvm.exe --jdk-only -cp regression-suite/classes RJdkNet
# expected: "CK RJdkNio asyncClose=AsynchronousCloseException",
#           "CK RJdkNet asyncClose=SocketException", PASS on all four.

cargo test -p cratonvm-native-io --lib net::tests
cargo test -p cratonvm-native-io --lib socket_channel::tests
```

## What would falsify the diagnosis

**The single observation:** run the two arms and look at `outcome`, which both
tests print. The diagnosis says the reader is parked in `recv` and the close
never reaches it.

* If a fixed binary reports `CK RJdkNio asyncClose=AsynchronousCloseException`
  and `CK RJdkNet asyncClose=SocketException`, the diagnosis held.
* If either still reports `none` (the initial value — the reader never returned
  at all), the read is parked somewhere other than these two natives. Check with
  `CRATONVM_DBG_SC_READ=1` / `CRATONVM_DBG_NET=1`: if `sc_read` / `read0` never
  fires for that fd, some other registration is servicing the read and this fix
  is aimed at the wrong layer.
* If either reports `returned` / `returned:-1`, the reader *did* wake — the
  wakeup path works and the residual defect is only the exception type, i.e.
  `closed_or_io_error` / `net_err` mapping, not the parking.

## The third reader — the synthetic surface (fixed 2026-08-11)

The 2026-08-07 pass named this as a residual and left it: "same fix applies if
that mode is ever exercised against an asynchronous close". It was, and it was
still live four days later.

`native-builtins/src/net_phase_e.rs::re1_socket_read_stream` had the identical
shape — clone the `Arc` out of `s2_registry`, drop the lock, park in
`(&*stream).read(..)` inside a bare EINTR retry loop with no close-awareness.
This is the **synthetic** `java.net.Socket` surface, selected by
`CRATONVM_SYNTHETIC_NET_SOCKETS` (equivalently `CRATONVM_REAL=-net-sockets`,
which is the spelling the binary now asks for). `io.real_net_sockets` is
default-ON, and `register_re1_socket` returns before registering anything when
it is set, so this whole surface — reader, `available()`, `close` — is absent
from Compatible mode.

### Measured 2026-08-11

`AsyncCloseProbe`, the `RJdkNet:224-253` asyncClose block lifted into a file of
its own. That extraction is necessary, not cosmetic: `RJdkNet` under synthetic
sockets dies at `RJdkNet.java:171` on a `TCP_NODELAY` check inside
`loopbackTcp`, which is upstream of the asyncClose block, so the assertion this
record is about is **unreachable through `RJdkNet` on that arm**. A run of
`RJdkNet` alone would have reported a red that says nothing about this defect.

Against `target/release/cratonvm.exe` as built that day — which already carries
the 2026-08-07 `net.rs`/`socket_channel.rs` fix:

| arm | woke | outcome |
| --- | --- | --- |
| HotSpot 25 | true | `SocketException` |
| CratonVM, default (Compatible) | true | `SocketException` |
| CratonVM, `CRATONVM_REAL=-net-sockets` | **false** | **`none`** |

`none` is the probe's initial value: the reader never returned at all, which is
the "read is parked and the close never reached it" signature this record's own
falsification section names. The default-mode row is the first execution of the
2026-08-07 fix on a binary — it holds.

`CRATONVM_DBG_SOCK=1` on the failing arm places the park precisely:

```
[dbg-sock] read: sid=2 want=1 (blocking on recv...)
[dbg-sock] read: sid=2 got=0
```

Two facts in three lines. `re1_socket_read_stream` is the native servicing the
read, so the fix is aimed at the right layer. And the `got=0` does not arrive on
`client.close()` — it arrives ~10 s later, when the try-with-resources closes
the *accepted* peer socket and its FIN delivers a genuine EOF, long after the
probe had given up. Even that late wakeup is the wrong answer: `n == 0` returns
`-1`, a clean end-of-stream, where `Socket.close()` mandates an exception.

### What JDK 25 specifies

`java.net.Socket.close()`, `src.zip` line 1593:

> Any thread currently blocked in an I/O operation upon this socket
> will throw a {@link SocketException}.

Unconditional — "will", not "may". Worth reading in 25 specifically rather than
from memory, because the surrounding contract did move: the neighbouring
`getInputStream()` javadoc now enumerates *interruptibility* cases that did not
exist in earlier releases (a `SocketChannel`-associated socket throws
`ClosedByInterruptException`; a virtual thread reading on the system-default
impl wakes, closes the socket, and throws `SocketException` with the interrupt
status set). None of those is this defect — the probe never interrupts anyone —
but they are why "what the JDK does on a blocked read" is a version-sensitive
question.

The concrete message comes from `sun.nio.ch.NioSocketImpl.endRead`, which runs
in the `finally` of `implRead`:

```java
if (!completed && state >= ST_CLOSING)
    throw new SocketException("Socket closed");
```

### The fix — the same mechanism, not a second one

Reused, not reinvented. `re1_read_close_aware` is `net_read_close_aware` /
`read_close_aware` in shape: park in `poll` with a bounded slice, re-ask the
registry, return `ErrorKind::Interrupted` when the slot is gone. The registry
question is even simpler here — `re1_close_socket` *removes* the entry, so
`streams.contains_key(sid)` is the flip. The lock is taken per pass for that
lookup alone and never held across the poll or the read.

The poll primitive is not a new one either. `net::poll_stream_readable` is
`pub(crate)` to `cratonvm-native-io` and `net_phase_e.rs` is in
`cratonvm-native-builtins`, so it cannot be called across the crate boundary —
but this file already bound `poll(2)`/`WSAPoll` for `SocketInputStream
.available()`, with the timeout hard-coded to 0. That binding became
`re1_socket_poll_readable(stream, timeout_ms)`, restating
`poll_stream_readable`'s exact three-state contract (`Some(Ok(true))` ready,
`Some(Ok(false))` timed out, `None` no primitive on this target → fall back to
one plain blocking read rather than spin on a stub). `re1_socket_read_ready` is
now a one-line timeout-0 wrapper, so `available()` keeps its behaviour and the
crate does not grow a fourth binding of one syscall (`servlet.rs` and
`xnio_conduits.rs` hold the other two).

One deliberate change inside the primitive: readiness is now `rc > 0` rather
than a `revents & POLLIN` mask test, because POLLERR/POLLHUP/POLLNVAL are
delivered whether or not they were requested and must count as ready — treating
them as not-ready would park a reader forever on a socket that can never become
readable, which is the failure mode this whole path exists to remove.
`available()` is unaffected in observable behaviour: its `peek` already mapped
both `Ok(0)` and `Err(_)` to 0.

### Two things the `net.rs` twin did not have to handle

**`SO_TIMEOUT` has to bound the park, not just the recv.** `net_read0` could
delegate the timed case entirely — `NioSocketImpl.timedRead` drives its own
`Net.poll` deadline and `net_read0` only touches the `can_park` branch. This
surface has no such caller: the synthetic natives *are* the impl, and
`setSoTimeout` here is an `SO_RCVTIMEO` on the socket plus a
`read_timeout_ms` in the side table. So `re1_read_close_aware` takes an explicit
deadline, clamps each poll slice to the time remaining, and raises `TimedOut`
(already mapped to `SocketTimeoutException`) when it expires. This is the
campaign's standing rule applied to a wakeup: a timeout that does not actually
end the wait leaves the thing running. A close-aware read that polled forever
would have fixed the hang on `close()` and introduced a new one on
`setSoTimeout`. `SO_RCVTIMEO` stays set and remains the first line; the deadline
is what still ends the park where `SO_RCVTIMEO` does not fire.

**The exception type has to be named here.** On the real-JDK surface
`NioSocketImpl.endRead` retypes whatever arrives to `SocketException("Socket
closed")`, which is why `net_read_close_aware` could return a generic error and
still land the right type. `NioSocketImpl` is never on this path. `RuntimeError`
has no `SocketException` variant — only the `ConnectException`/`BindException`/
`SocketTimeoutException` subclasses — so `re1_socket_exception` constructs
`java/net/SocketException` through `new_object_initialized`, the same way
`re1_socket_write_stream` in that file already does for a peer reset. The type
is load-bearing: `RJdkNet:248` asserts `outcome.equals("SocketException")`, and
its `catch (SocketException)` arm sits above `catch (IOException)`.

### Platform split, and what could not be compiled here

Three `cfg` arms were edited, all in `re1_socket_poll_readable`:

* `#[cfg(unix)]` — `libc::poll`. **Not compilable on this Windows host**, in
  principle as well as in practice. EINTR is reported as "not ready", never as
  an error and never as an in-place re-poll: `poll(2)` is never auto-restarted
  by `SA_RESTART`, so a signal delivered to a parked thread always returns
  EINTR, and this VM sends one on purpose (`jit::xt_root_scan` SIGUSR2s every
  thread for a cross-thread root scan). Reporting "not ready" is also what keeps
  the `SO_TIMEOUT` deadline honest — re-polling in place with the same
  `timeout_ms` would restart the whole wait on every GC. This mirrors
  `net.rs::net_poll_raw`'s AUDIT 2026-08-02 arm exactly.
* `#[cfg(windows)]` — `WSAPoll`. The arm this host builds. No EINTR arm, because
  Winsock has no EINTR and therefore no `SA_RESTART` hazard.
* `#[cfg(not(any(unix, windows)))]` — returns `None`. **Not compilable on any
  host in this campaign.**

The platform split is also why this residual survived a records audit: on Linux
`shutdown(SHUT_RD)` does wake the parked `recv`, so the reader returns rather
than hanging — with `-1`, which is still wrong but does not look like a hang.
Only Windows shows the full failure. Both are fixed by the registry re-ask,
which runs before the read on every pass.

### Mode

Synthetic-socket mode only. Compatible mode is byte-for-byte unchanged:
`register_re1_socket` returns early under the default `io.real_net_sockets`, so
none of the touched code is registered there — and the default-mode probe row
above was measured on the unmodified binary for the same reason.

`native-builtins/src/plain_socket.rs` was inspected and **needs no change**: the
`PlainSocketImpl`/`NioSocketImpl` surface it registers has no read native at all
(`socketCreate`/`Connect`/`Bind`/`Listen`/`Accept`/`Close0`/`Shutdown`/
`SetOption`/`GetOption`/`Available`/`SendUrgentData` and two no-op initialisers),
so it has no reader that could park. Its `socketAccept` already carries the
close-aware poll loop.

### Out-of-file patch (not applied)

**Not required to compile** — the fix above is self-contained in
`net_phase_e.rs`. This is the consolidation that would leave one poll primitive
in the tree instead of two, and it touches a file this lane does not own.

`native-io/src/net.rs`, line 2421:

```rust
-pub(crate) fn poll_stream_readable(
+pub fn poll_stream_readable(
```

With that, `net_phase_e.rs` can delete all three `re1_socket_poll_readable` arms
and call `cratonvm_native_io::net::poll_stream_readable` directly —
`native-builtins` already depends on the crate and already calls
`cratonvm_native_io::net::take_stream_for_tls`, so no new dependency edge is
created. Worth doing only together with the same collapse in `servlet.rs` and
`xnio_conduits.rs`, which hold the crate's other two `WSAPoll` bindings;
converting one site of a four-site idiom is how these grow back.

## How to prove the wakeup lands

Nothing below has been run against a binary containing the 2026-08-11 change —
none was built.

The probe is `AsyncCloseProbe` above, and the single observation is the
`outcome` line. Run all three arms; the first two are the controls that say the
instrument works:

```
java -cp <classes> AsyncCloseProbe                                    # HotSpot 25
cratonvm.exe -cp <classes> AsyncCloseProbe                            # Compatible
CRATONVM_REAL=-net-sockets cratonvm.exe -cp <classes> AsyncCloseProbe # synthetic
```

`PROBE outcome=SocketException` on all three is the fix.

Read the failures the way this record's falsification section already sets out,
with one addition specific to this surface:

* `outcome=none` — the reader is still parked; the poll loop is not running or
  is not seeing the registry removal. `CRATONVM_DBG_SOCK=1` should now print
  `(parking in poll...)`; if it still prints `(blocking on recv...)`, the binary
  predates this change.
* `outcome=returned:-1` — the reader woke but on an EOF rather than on the
  registry re-ask. That is the pre-fix Linux answer, and it means the ordering
  argument failed: the read ran before the `re1_stream_still_registered` check
  rather than after it.
* `outcome=SocketTimeoutException` — the deadline arm fired on a socket with no
  `SO_TIMEOUT` set. `deadline` should be `None` unless `read_timeout_ms > 0`.

The `SO_TIMEOUT` half must not regress, and it has its own existing two-mode
harness: `vm/tests/socket_input_stream_timeout.rs` runs the
`SocketInputStreamTimeout` fixture across `real_net_sockets` on and off, which
is exactly the pair of arms this change straddles. Run it before believing the
asyncClose result.

`RJdkNet` itself will still fail on the synthetic arm at the `TCP_NODELAY` check
on line 171 — a separate defect, upstream of this one, and not evidence about
this record either way.
