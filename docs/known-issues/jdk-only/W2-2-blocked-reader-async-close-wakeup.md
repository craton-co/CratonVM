# A blocked reader never woke up — `close()` cannot reach a thread parked in `recv`

**Status:** FIXED (code landed 2026-08-07, unverified — no binary was built in
the session that wrote it). Lane W2-2 of the jdk-wave2 pool.

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

## Residual, not fixed here

`native-builtins/src/net_phase_e.rs::re1_socket_read_stream` (line ~3645) has
the identical shape: it clones the stream out of `s2_registry`, drops the lock,
and parks in `(&*stream).read(..)` with no close-awareness. That is the
**synthetic** `java.net.Socket` surface, reachable only under
`CRATONVM_SYNTHETIC_NET_SOCKETS`, so it is on neither failing mode's path — and
the file belongs to another lane. Same fix applies if that mode is ever
exercised against an asynchronous close.
