# W7-53 — the blocking-close-awareness family: 19 sites fixed, 7 named open, and a census that was short in both directions

**Branch:** `fix/blocking-close-awareness-family-20260812`, based on dev
`054348ec0`.

**Nothing in the Rust half of this record was built or run.** This session had
no build. Every "before" is a reading of the tree; every "after" is a claim
about source. The one thing that WAS executed is the probe, against HotSpot
25.0.3 on this Windows 11 host — see "The instrument" below, and read that
section before believing any row here.

Predecessors: `W2-2-blocked-reader-async-close-wakeup.md` (the three readers,
fixed 2026-08-07 and 2026-08-11), and `W7-47-w2-cluster.md`, whose census of
this family is the input to this lane.

---

## The contract

A thread parked in a blocking read/write/accept on a socket, channel or pipe
must come back when another thread closes it. JDK 25's `java.net.Socket.close()`
is unconditional — "Any thread currently blocked in an I/O operation upon this
socket will throw a `SocketException`", *will*, not *may* — and the
`java.nio.channels` types say the same with `AsynchronousCloseException`. Where
it does not hold, `close()` from a second thread does nothing, the blocked
thread stays blocked forever, and shutdown hangs.

**Classification, once, for the whole lane.** No `ClassOrigin` is involved and
`--jdk-only` changes nothing about any of it: every registration touched is
`NativeKind::Bridge`, so `drop_synthetic_stubs` does not reach it. These are
ordinary **Compatible-mode defects that strict inherits unchanged**. Two of the
files are mode-gated for a different reason, stated per row below
(`net_phase_e.rs` is synthetic-socket mode only; `servlet.rs`'s `s2` surface is
the synthetic NIO one).

---

## Census: mine against W7-47's

W7-47 measured "~20 blocking sites, 8 files", named three as already fixed, fixed
`DatagramChannel.receive`'s lock half itself, and handed over "the remaining
~19". Re-deriving it from the tree rather than from the list gives **19 sites
fixed here** — the same number — but **not the same nineteen**. Eight sites are
new, and two of the census's rows do not survive contact with the code.

### Fixed here (19)

| # | site | file | how it could not see the close |
|---|---|---|---|
| 1 | `FileDescriptorTable::udp_recv` | `native-api/src/fd_table.rs` | chokepoint; 4 callers |
| 2 | `FileDescriptorTable::tcp_read` | `native-api/src/fd_table.rs` | chokepoint; 4 callers. **Not on the census** |
| 3 | `FileDescriptorTable::tcp_write` | `native-api/src/fd_table.rs` | **Not on the census** |
| 4 | `FileDescriptorTable::tcp_accept` | `native-api/src/fd_table.rs` | **Not on the census** |
| 5 | `net_write0` | `native-io/src/net.rs` | the missing write twin |
| 6 | `re1_socket_write_stream` | `native-builtins/src/net_phase_e.rs` | the missing write twin |
| 7 | `s2_blocking_accept` | `native-builtins/src/servlet.rs` | parks on a `try_clone`d duplicate |
| 8 | `SocketChannel.read` closure | `native-builtins/src/servlet.rs` | streams left blocking by row 7 |
| 9 | `SocketChannel.write` closure | `native-builtins/src/servlet.rs` | as above |
| 10 | `DatagramChannel.read` | `native-builtins/src/phases_late/net_channels.rs` | **Not on the census** — and it carried the census's own worst defect too |
| 11 | `Job::Accept` | `native-io/src/async_socket.rs` | worker consumed permanently |
| 12 | `Job::Write` | `native-io/src/async_socket.rs` | |
| 13 | `Job::ReadFd` | `native-io/src/async_socket.rs` | private `try_clone`d handle |
| 14 | `Job::ReadFutureFd` | `native-io/src/async_socket.rs` | as above |
| 15 | `Job::WriteFutureFd` | `native-io/src/async_socket.rs` | as above |
| 16 | `source_read_buffer` | `native-io/src/pipe.rs` | handle copied by value |
| 17 | `source_read_bytes` | `native-io/src/pipe.rs` | as above |
| 18 | `sink_write_buffer` | `native-io/src/pipe.rs` | as above; Windows arm partial, see below |
| 19 | `sink_write_bytes` | `native-io/src/pipe.rs` | as above; Windows arm partial, see below |

### Named open (7) — see "What is left" for each

`DatagramChannel.receive`; four TLS stream sites; the multi-acceptor race in
`s2_blocking_accept`; the Windows arm of the pipe sink write.

### Two census rows that do not survive the code

**"Datagram send, a chokepoint with three callers."** The census text says
*receive*, and receive is right; the handover paraphrased it as *send*. A UDP
`send` does not wait on a peer — it is bounded by the socket's own send buffer,
returns, and has no party that could fail to arrive. `udp_send` and
`udp_send_connected` are **not** defects and were left alone. Fixing them would
have been a change to a path with no failure mode, which is how a census grows
rows that later have to be un-fixed.

**"Pipe: the handle is copied by value, so removing the map entry does not close
the handle the parked thread holds."** The entry is not removed and the handle
*is* closed. `close_pipe_end` set `closed = true` and called `close_raw(end.raw)`
immediately, on the same raw handle a parked `ReadFile`/`read(2)` was using. So
the defect is **worse** than the row says, not milder: not merely an unwakeable
park but a use-after-close the instant the OS recycles the handle number onto an
unrelated file. That correction is what decides the mechanism — see below.

### What the census did not look at, and why the misses cluster

The three `fd_table` misses and the `DatagramChannel.read` miss have one shape
in common: **the census read the files it had already named and did not re-ask
which files reach a blocking syscall.** `fd_table.rs` was named once, for
`udp_recv`, and its three TCP siblings sit within eighty lines of it.
`DatagramChannel.read` sits one screen below `DatagramChannel.receive` in the
file the census edited, and carries the same process-wide-lock wedge that the
census called "the worst" — `recv` ran with the `s2_registry` guard alive. The
four TLS sites are in two more files that were never opened.

Not defects, checked and cleared here as well: `Selector.select` (a UDP loopback
wakeup socket is in the wait set and `selector_close` nudges it);
`xnio_conduits`' transports (set non-blocking at every accept/connect site,
though nothing in that file asserts it, so it stays latent);
`socket_channel.rs`'s read/write/accept and `net.rs`'s accept (already
close-aware); `async_socket`'s `try_read_ready_bytes` (gated on an availability
query, so it cannot park); `fd_table`'s `udp_send*`.

---

## The mechanism — one loop, reused, not a second one

All 19 use the shape the three landed readers use:

> park in `poll`/`WSAPoll` on a bounded slice; re-ask the registry **after** the
> poll; return `ErrorKind::Interrupted` once the slot is gone.

Asked *after* the poll on purpose: a close landing while parked is then seen on
the very next pass, and a close that raced a readiness edge still wins — HotSpot
fails an I/O a concurrent `close()` beat, it does not hand back bytes on a socket
Java has already closed.

`ErrorKind::Interrupted` is the carrier everywhere, and it is unambiguous at
every site for the same reason: each path reissues every real EINTR itself, and
every poll primitive here reports EINTR as *not ready* rather than as an error.

**No new binding of `poll(2)`/`WSAPoll` was added.** The census counted seven in
the tree and warned that a consolidation planned against four would leave three
behind. Rather than add an eighth:

* `fd_table.rs`'s existing zero-timeout `poll_readiness` grew a `timeout_ms`
  parameter; the old entry point is now a one-line wrapper, so `poll_ready` is
  unchanged.
* `net.rs` gained `poll_stream_writable`, routed through the same
  `net_poll_stream` as `poll_stream_readable` with `NET_POLLOUT`.
* `net_phase_e.rs`'s single binding grew a direction parameter;
  `re1_socket_poll_readable` keeps its name and contract, so `available()` is
  untouched. **Superseded 2026-08-12: that binding is now GONE.** W2-2's collapse
  landed — `re1_socket_poll_readable` / `re1_socket_poll_writable` are one-line
  calls into `cratonvm_native_io::net`, so the crate is down one poll binding
  rather than holding one that had to be kept in agreement by hand. The seven-way
  count in the row below drops by one; `servlet.rs` and `xnio_conduits.rs` still
  hold theirs.
* `servlet.rs` used its own existing `selector_poll`/`PollReq` abstraction.
* `async_socket.rs` and `net_channels.rs` call into the above.

### `SO_RCVTIMEO` vs `SA_RESTART`, stated per regime

The trap this area is documented for, answered explicitly:

* **What the wakeup depends on.** Not on signals and not on `SO_RCVTIMEO`. It
  depends on `poll`/`WSAPoll` returning at its own timeout, and on the registry
  answer changing. Neither regime is load-bearing for the wakeup itself.
* **Where `SA_RESTART` matters.** On Unix, `poll(2)` is *never* auto-restarted
  by `SA_RESTART`, so a signal delivered to a thread parked in it always returns
  EINTR — and this VM sends one on purpose (`jit::xt_root_scan` SIGUSR2s every
  thread for a cross-thread root scan). Every Unix arm added here therefore
  reports EINTR as **not ready** and never re-polls in place: re-polling with the
  same `timeout_ms` would restart the whole wait on every GC and silently defeat
  any deadline above it. Winsock has no EINTR and therefore no `SA_RESTART`
  hazard, so the Windows arms have no such branch.
* **Where `SO_RCVTIMEO` matters, and why it had to be re-derived.** A syscall we
  no longer issue until the socket is ready is a syscall `SO_RCVTIMEO` can never
  bound. Left alone, this fix would have removed one hang and introduced another
  on every timed read. So every site that can carry a timeout reads it back off
  the socket (`read_timeout()`/`write_timeout()`) and enforces it as the loop's
  own deadline: `fd_table`'s four, and `async_socket`'s `aio_read_close_aware`
  (the timed `AsynchronousSocketChannel.read` overload sets a real read timeout
  on the worker's private clone). `SO_RCVTIMEO` stays set and remains the first
  line; the deadline is what still ends the park where it cannot fire.

### On expiry — what every bounded wait added here does

Two different bounds, and only one of them is an outcome:

* The **per-pass slice** (25 ms; 5 ms in `pipe.rs`, for a reason given below)
  expiring is **not** an outcome. It is the point at which the registry is
  re-asked and the loop continues. Nothing is reported, nothing is abandoned.
* The **caller's deadline**, where one exists, **is** an outcome: it ends the
  wait with `TimedOut`, which each surface maps to the `SocketTimeoutException`
  it already specified. It does not merely decline to poll again.

No expiry anywhere leaves an operation running.

### Non-blocking mode is untouched everywhere

Every site gates the close-aware path on the recorded blocking mode and leaves
the non-blocking path byte-for-byte alone. That is not caution for its own sake:
parking a non-blocking socket would take the JDK's `IOStatus.UNAVAILABLE` (-2)
protocol away from its caller, which is every selector-driven reactor in
Netty/Tomcat/Jetty. `fd_table` had no record of the mode, so it gained one — a
side table written by the two `set_nonblocking` entry points and dropped on
`close`, modelled exactly on `net.rs::net_pending_nonblocking`, and answering
"blocking" for an unknown fd for the same deliberate reason (guessing "cannot
park" for a socket that can is the unsafe direction).

---

## Three things fixed that were not on the census's list

**A half-duplex wedge in `fd_table::tcp_read`.** It held the per-fd `Mutex`
across the whole blocking read, so every `tcp_write` on the same fd queued behind
a reader waiting on a peer that might never speak. `try_clone_tcp` exists only
to work around it. The poll now runs outside that mutex, which is taken only
once the socket is already readable.

**A process-wide wedge in `DatagramChannel.read`.** `recv` ran with the guard on
the process-wide `s2_registry` mutex still alive, because the call sat inside the
block that owns it — the same self-sustaining wedge W7-47 called the worst thing
it found in `receive`, one screen up in the same file, and did not check for
below it. On a blocking datagram channel that parks every synthetic socket
operation in the VM until a packet arrives, *including the close that would end
the wait*.

**A platform claim in `async_socket.rs` that is false on this host.**
`aio_shutdown_stream` and `aio_shutdown_fd_table_stream` issue `shutdown(Both)`
before removing the entry, and their doc comments state without qualification
that this "unblocks *every* fd that still references the same
open-file-description, including clones already handed to worker threads". True
on Linux, where `SHUT_RD` wakes a parked `recv` with EOF. **False on Windows**,
where no `shutdown` aborts a pending blocking call — only `closesocket` does, and
that code cannot close a handle a worker is mid-syscall on. The claim is
load-bearing: it is why nothing else was ever added there. The shutdown stays (it
is the cheaper wakeup where it works) and a cancellation flag was added beside
it.

---

## `pipe.rs` — why the registry re-ask cannot be lifted, and what replaces it

The census is right that the mechanism does not transfer. It is right for a
reason one step further along than the one it gives.

Every socket site in this family parks holding an `Arc<TcpStream>` cloned out of
a registry. **Two** things follow, and both are load-bearing:

1. the OS handle cannot be closed while the thread is parked, because the `Arc`
   is still alive — so `close` can only *mark* the registry; and
2. marking the registry is therefore a safe, purely advisory question the parked
   thread may re-ask on any cadence it likes.

Neither holds for a bare `u64` with no ownership attached. `PipeEnd` is `Copy`,
`pipe_end_get` handed out a snapshot, and `close_pipe_end` freed the handle in
that snapshot immediately. A registry re-ask bolted onto that would have been a
question whose answer arrives after the damage.

So the fix is **two halves**, and neither is sufficient alone:

* **`in_flight` / `close_pending`** — an ownership discipline that gives the
  handle the lifetime the socket sites get from their `Arc`. A thread announces
  itself with `pipe_enter` and retires with `pipe_leave`; `close` sets `closed`
  at once but **defers `close_raw` to the last thread out**. Without this half
  the generation answer arrives too late to matter, and the use-after-close
  remains.
* **the `closed` generation, re-read through `pipe_still_open`** between bounded
  readiness probes. Without this half, `in_flight` alone would only make an
  unbounded park *safe* instead of *ending* it.

The readiness probe is what keeps the thread out of the syscall long enough to
ask at all. Unix: `poll(2)` on the fd, both directions. Windows read side:
`PeekNamedPipe`, which is documented to work on anonymous pipes and needs only
the `GENERIC_READ` access `CreatePipe`'s read handle already has — a pure query
that consumes nothing and changes no handle state, so unlike a
`SetNamedPipeHandleState(PIPE_NOWAIT)` dance it cannot race a concurrent reader
into a spurious short read. Because it does not itself wait, the bound is a
`Sleep` between probes, which is why `PIPE_CLOSE_POLL_MS` is **5 and not 25**:
there it is a real latency cost rather than only a liveness bound. On Unix it is
a genuine `poll` timeout and costs nothing.

A close now raises a real `AsynchronousCloseException` rather than a bare
`IOException` whose message mentions the name. The concrete type is what
`java.nio.channels` callers catch, and a message-prefix `IOException` lands in
the wrong arm — the same mistake `sc_read` used to make.

### The one row left open rather than falsely closed

**The Windows sink write.** Windows offers no space-available query for the
write end of a pipe. The two mechanisms that would give one both *change how the
handle behaves* rather than observing it: `SetNamedPipeHandleState(PIPE_NOWAIT)`
(documented as legacy LANMAN compatibility, and it alters every write on the
handle), or creating the pipe with `CreateNamedPipe(FILE_FLAG_OVERLAPPED)` +
`CreateFile` instead of `CreatePipe` and using a bounded `GetOverlappedResultEx`.
The second is the correct fix and is the named follow-up; neither is worth
landing on inspection, without a build to run it against.

So `poll_pipe` answers `None` for a write on Windows, which routes to one plain
blocking `WriteFile` — the pre-existing behaviour — with the generation check
still applied *before* it and the deferred close still applied *after*. A
Windows sink write therefore no longer risks a use-after-close, and does observe
a close that has already happened; a close arriving while it is inside
`WriteFile` is what it still cannot see.

Answering `Some(Ok(false))` there instead would have compiled, looked exactly
like the other three, and spun a loop that could never report readiness while
every caller believed it was close-aware. That is precisely the shape that
removes a site from a census while leaving the defect, and it is why this row is
written down rather than quietly counted.

---

## The instrument

`probes/AsyncCloseProbe.java`, with its HotSpot oracle transcript at
`probes/AsyncCloseProbe.expected.txt`.

> **It has never run in any suite, and it cannot.** `regression-suite/run.sh`
> compiles `"$HERE"/src/*.java` and runs a hand-maintained word list
> (`CORE_CLASSES` / `JDKONLY_CLASSES`); nothing in it reads `probes/` at all. So
> this instrument is in exactly the position `run.sh`'s own comment describes —
> *"a `src/*.java` vector named in no list … looks like coverage and is not"* —
> one step worse, because it is not even in `src/`. Everything the section below
> says about it is true of a hand invocation; **none of it is scheduled**, and
> the standing rule for this campaign is that an assertion outside a scheduled
> fixture cannot close a record.
>
> Scheduling it is a two-part edit neither of which belongs to a
> `native-io`/`native-builtins` lane: move the file to
> `regression-suite/src/RAsyncClose.java` (the glob then compiles it) and add
> that name to `CORE_CLASSES` — **core, not `JDKONLY_CLASSES`**, because this
> family is a Compatible-mode defect that strict merely inherits, as the
> Classification section above states. Two things to check when doing it: the
> probe calls `System.exit`, which run.sh's `PASS <Class>` grep tolerates but
> which must still emit the `PASS RAsyncClose (N checks)` banner the harness
> guards look for; and its `-Dprobe.*` properties must have safe defaults,
> because the suite passes no `-D` of its own.

### Two of the thirteen shapes are now SCHEDULED — 2026-08-12

The move-and-schedule above is still not done, and the coverage rule ("an
assertion outside a scheduled fixture cannot close a record") therefore still
holds against this record. What changed is that the two shapes with **no**
scheduled assertion anywhere and a fix in this wave now have one, inside a
fixture `run.sh` already names: `regression-suite/src/RJdkNet.java`, new section
`asyncCloseWriteAndAccept()`, 8 checks (72 → **80**).

* **write** — a writer parked in `Socket.getOutputStream().write(..)` behind a
  peer that never reads, woken by `Socket.close()` on another thread. This is
  row 5/6 of the fixed table (`net_write0` / `net_write_close_aware`, landed in
  this wave) and it **fails on the pre-fix behaviour**: before it, the writer
  stayed in `send` and the row's bounded `await` expires.
* **accept** — an acceptor parked in `ServerSocket.accept()`, woken by
  `ServerSocket.close()`. `net_accept_close_aware` has carried this since
  2026-05-17 and nothing scheduled has ever asserted it; it is a ratchet, not a
  new fix.

Three things about the pair that are the same discipline the probe uses, and one
that is new:

* the surface is the **shipping** one — `real_net_sockets` is default-ON, so both
  rows run real JDK bytecode down to `sun/nio/ch/SocketDispatcher.write0` and
  `sun/nio/ch/Net.accept`, both `register_with_kind(.., NativeKind::Bridge)` in
  `native-io`'s `net::register_sun_nio_ch_net` ←
  `nio_native::register_t16_channel_overrides` ← `register_io_natives`, which
  `vm_init` calls on **all three** boot arms. `Bridge` is not a kind `JdkOnly`
  drops, so strict inherits the same bodies;
* the park is **proved before the close** (`...WasBlocked`, from the latch count)
  and reported as its own failure, so a row whose worker had already returned
  cannot pass;
* every wait is bounded by the file's own `T`, so an unfixed native yields a
  **FAIL, not a suite timeout**;
* and, new: the write row closes the **accepted** end in a `finally` *before* it
  asserts. That releases the writer through the peer's reset even on a VM where
  the close is invisible to it, so a red row cannot leave a thread parked in the
  kernel at exit. `AsyncCloseProbe` solves the same problem with `System.exit`,
  which a suite vector must not call.

**Re-checked 2026-08-12 and one sentence above is wrong in a way that matters.**
This record's Classification section says the family is a Compatible-mode defect
that strict merely inherits, and its scheduling note above says the probe belongs
in `CORE_CLASSES`, "**core, not `JDKONLY_CLASSES`**", for exactly that reason.
But the two shapes that *were* scheduled went into `regression-suite/src/
RJdkNet.java`, and `RJdkNet` appears **only** in `JDKONLY_CLASSES`
(`regression-suite/run.sh:119`) — it is not in `CORE_CLASSES`
(`run.sh:106`). So `asyncCloseWriteAndAccept` runs in the strict arm alone, and
**this family still has zero scheduled cover in Compatible mode**, which is the
shipping default. Either the rows move to a core fixture or `RJdkNet` is added to
`CORE_CLASSES`; both are `run.sh`/fixture edits outside this record's lane. Not a
defect in the rows — they are real assertions where they run — but the coverage
claim above overstates their reach.

`SocketException` is the assertion on both VMs and it does not depend on which
error the native picks: JDK 25's `NioSocketImpl.implWrite` catches every
`IOException` from the dispatcher and rethrows `asSocketException(ioe)` ("throw
SocketException to maintain compatibility"), and `endWrite`/`endAccept` throw
`SocketException("Socket closed")` from their `finally` whenever the call did not
complete and the impl is `>= ST_CLOSING` (read from `src.zip`, JDK 25.0.3.9). What
the native must do is **return**; the type is the JDK's.

**No TLS row was added to `RJdkNet` and one must not be**: the four TLS sites'
Windows half is open, so a TLS row would be a scheduled RED on this host. The
place for it is `AsyncCloseProbe`'s `tlsRead`/`tlsWrite`, which W7-61 added.

W2-2 records a 2026-08-11 measurement taken with an instrument it calls
`AsyncCloseProbe`. **That file was never in the repository** — not in `probes/`,
not in `regression-suite/src/`. The measurement is real; the instrument is not
reproducible from the tree, which for a suite is the same as not having measured
it. W7-47 recorded the gap and declined to fill it, on the grounds that writing a
probe nobody could run would produce an instrument nobody has seen agree with a
control. That objection is answered by running the control, which this session
could do.

13 rows, one per Java-reachable site in the family: `socketRead`, `socketWrite`,
`serverSocketAccept`, `channelRead`, `channelWrite`, `channelAccept`,
`datagramReceive`, `datagramChannelReceive`, `pipeRead`, `pipeWrite`,
`asyncAccept`, `asyncRead`, plus `selfTestNoPark`.

### How it avoids the two shapes that make such a probe worthless

**It does not close before the read blocks.** A close issued ahead of the block
tests nothing: the read then returns immediately for an unrelated reason. Every
row proves the park first — the worker publishes `entered` immediately before the
blocking call, the driver waits for that flag, sleeps 300 ms (12 slices at the
25 ms close-poll cadence), and asserts the worker has **not** returned. A row
that returned inside that window is reported `INCONCLUSIVE`, never `PASS`, and
fails the run.

**It does not hang on failure.** A probe that hangs converts a red into a stuck
job. Every worker is a daemon thread; every wait is bounded
(`join(probe.wakeMs)`, 4 s); an expired row reports `outcome=TIMEOUT` and FAILS;
nothing is retried; each row closes its sockets in a `finally` whether it passed
or not, so an expired row cannot contaminate a later one; and `System.exit`
guarantees the JVM leaves with workers still parked in the kernel.

### Both guards are calibrated, not asserted

* **`selfTestNoPark`** is a negative control — a read whose peer has already sent
  a byte, so it cannot block. It PASSES by *not* parking. Without it,
  `parked=true` on the other twelve rows would rest on a check nobody has seen
  say `false`, which is the standing the missing probe left W2-2's measurement
  in. If that row ever reports `parked=true`, no other row in the file means
  anything however green.
* **`-Dprobe.skipClose=<row>`** suppresses the close so the failure path can be
  executed on any VM. Measured on HotSpot 25.0.3 / Windows 11, 2026-08-12:
  `socketRead` FAILs at `ms=4000`, the run exits 1, wall clock 5.1 s. A harness
  whose failure path has never been executed is not known to have one.

### Measured 2026-08-12 — HotSpot 25.0.3, Windows 11

**13 pass, 0 fail, 0 inconclusive, exit 0.** Full transcript in the
`.expected.txt`. One `want` was corrected against the measurement rather than the
row reshaped to agree with a guess: `channelWrite` answers
`ClosedChannelException`, because the parked write returns its partial count and
the *next* iteration of the row's write loop finds the channel already closed.
All of `AsynchronousCloseException` / `ClosedChannelException` / a partial count
are wakeups; only `TIMEOUT` is the defect.

**There is no CratonVM arm and none should be inferred.** No binary was built.

---

## How to run it

```
javac -d <out> probes/AsyncCloseProbe.java

java -cp <out> AsyncCloseProbe                              # HotSpot 25 — CONTROL, run first
cratonvm.exe -cp <out> AsyncCloseProbe                      # Compatible (--real-jdk)
cratonvm.exe --jdk-only -cp <out> AsyncCloseProbe           # strict
CRATONVM_REAL=-net-sockets cratonvm.exe -cp <out> AsyncCloseProbe   # synthetic java.net.Socket

# calibration, on whichever VM is under test:
java -Dprobe.skipClose=socketRead -cp <out> AsyncCloseProbe selfTestNoPark socketRead
#   expected: socketRead FAIL at ms=4000, exit 1, and the process leaves.
```

Run the HotSpot arm every time, not once. It is the control that says the
instrument works on that host; a CratonVM row is evidence only after the same
row has been seen to pass there.

Rust-side tests, none of which have been compiled:

```
cargo test -p cratonvm-native-io --lib pipe::tests
```

`a_close_wakes_a_reader_parked_on_a_pipe`,
`a_close_aware_pipe_read_still_delivers_bytes` and
`a_close_during_an_in_flight_op_defers_the_raw_close` are RED by construction
against the tree as it stood at `054348ec0`. The first proves the park before the
close in the same way the Java probe does: the reader publishes a started flag,
the closer waits for it plus a further 100 ms (20 poll slices) and asserts
`!is_finished()` before closing anything.

> **VERIFIED AGAINST A BINARY 2026-09-02.** Both handles this section names were
> run, on a build from this tree:
>
> ```text
> cargo test -p cratonvm-native-io --lib socket_channel::tests    19 passed, 0 failed
> cargo test -p cratonvm-vm --test socket_input_stream_timeout     1 passed, 0 failed
> ```
>
> The `socket_channel::tests` are the ones this section says "must stay green —
> they cover the three readers this lane did not touch", and they do. The
> `SO_TIMEOUT` fixture runs across `real_net_sockets` on and off, which is the
> pair this record cares about.
>
> Both handles were run because this record names two. Running one and inferring
> the other is the shape that made `H3-1` insist on "run both, paste both" — and
> here the two live in different crates and different build configurations, so a
> pass in one says nothing about the other compiling.

`cargo test -p cratonvm-native-io --lib net::tests` and
`--lib socket_channel::tests` must stay green — they cover the three readers this
lane did not touch. `vm/tests/socket_input_stream_timeout.rs` runs the
`SO_TIMEOUT` fixture across `real_net_sockets` on and off, which is the pair of
arms the `net_phase_e.rs` change straddles; run it before believing the
synthetic-socket row, because a deadline regression there is exactly the failure
this change had to design around.

---

## What would falsify this

The single observation is the probe's `outcome` column, per row, per arm.

* `outcome=TIMEOUT` on a CratonVM arm for a row in the fixed table — the fix is
  not reached. Check the site is the one servicing the call
  (`CRATONVM_DBG_SOCK=1`, `CRATONVM_DBG_SC_READ=1`, `CRATONVM_DBG_NET=1`); if the
  native never fires, some other registration is servicing it and the change is
  aimed at the wrong layer.
* `outcome=returned:-1` — the reader woke on an EOF rather than on the registry
  re-ask. That is the pre-fix Linux answer and means the ordering argument
  failed: the syscall ran before the registry check rather than after it.
* `parked=false` on any row but `selfTestNoPark` — the row tested nothing and its
  neighbours' verdicts are not evidence either until it is fixed.
* `selfTestNoPark parked=true` — the anti-vacuity guard is broken and the whole
  file's output is void.
* `pipeWrite outcome=TIMEOUT` on Windows CratonVM is the **expected** reading of
  the row this record leaves open, not a surprise.
* Any `SocketTimeoutException` on a socket with no timeout set — a deadline arm
  fired that should have been `None`. That is the `SO_RCVTIMEO` regression this
  change was designed against, and it would mean the deadline is being derived
  where no timeout exists.

---

## What is left

| what | where | why not here |
|---|---|---|
| `DatagramChannel.receive` close-awareness | `native-builtins/src/phases_late/net_channels.rs` | The lock half is the census branch's edit (`fix/w2-stream-stack-blocked-reader-moduledesc-20260812`), not yet on dev. Re-doing it here would only produce a conflict. Once it lands, add the same `s2_wait_ready_close_aware` call this branch added to `DatagramChannel.read` twenty lines below it — the helper is already `pub(crate)` |
| Windows pipe sink write | `native-io/src/pipe.rs` | Needs `CreateNamedPipe(FILE_FLAG_OVERLAPPED)` + a bounded `GetOverlappedResultEx`, i.e. a change to how the pipe is created. Not landable on inspection. **NARROWED 2026-08-12, still open — see "The Windows pipe sink write, narrowed" below** |
| `s2_tls_read_direct`, `s2_tls_write` | `native-builtins/src/servlet.rs` | TLS record layer. A close-aware loop must not abandon a read mid-record, so the wakeup has to be expressed against the *underlying* socket while the record assembler keeps its state. Genuinely a different problem, and not one to solve without a build. **Designed 2026-08-12 — see "The four TLS sites" below; the design is written down and NOT applied.** Two halves of it DID land (W7-61: a `shutdown` on the registry-held duplicate, which is the whole wakeup on Unix, plus an after-the-call classifier); what is open is the **Windows** arm, and on the third pass the remaining design was found unsound as written — "Third pass" below, trap 5 |
| `rustls_stream_read`, `rustls_stream_write` | `native-builtins/src/t27_tls.rs` | as above. Both already have the correct lock discipline; it is only close-awareness they lack. **DESIGNED IN FULL 2026-08-12 — `docs/feature-designs/jdk-only-tls-async-close.md`; read that before this section, and see "Fourth pass" below for what it corrects here.** **Same design; the "no poll binding of its own" obstacle is GONE** — `cratonvm_native_io::net::poll_stream_readable` is `pub`. `rustls_stream_read`'s CLIENT arm is the **pilot** the third pass recommends: no `cfg` arms, one exact screen (`conn.wants_read()`), and its reading decides the other three. `rustls_stream_write` must get NO loop — W7-61 measured HotSpot NOT waking a parked TLS write |
| multi-acceptor race in `s2_blocking_accept` | `native-builtins/src/servlet.rs` | With two threads accepting one listener, the loser of the race between the poll and the `accept()` parks again, not close-aware. Not closed by flipping the clone non-blocking: `try_clone` shares the blocking mode with the registry's listener on both platforms. Every accept parked before; at most one loser parks after |
| the seven-way poll-binding consolidation | tree-wide | W7-47's correction stands and this lane did not take it on. No eighth binding was added, and three of the seven grew parameters instead. **Two of the seven became callable across the crate boundary on 2026-08-12** — `net::poll_stream_readable` and `net::poll_stream_writable` are now `pub`, which is the precondition W2-2's collapse was blocked on |

---

## The four TLS sites — the design, written down and NOT applied

**Nothing in this section is applied. No file named here was edited.** All four
sites are outside the lane that wrote it, and the whole point of writing it down
is that the record already says *"do not fabricate a wakeup that does not wake
anything"* — so the alternative to a design is a design someone else invents
under time pressure.

### The one structural fact that makes it tractable

The reason this row reads as "genuinely a different problem" is the assembler:
`rustls::StreamOwned<Connection, TcpStream>` and
`native_tls::TlsStream<TcpStream>` both **own the socket inside them**, and both
are behind a per-stream `Mutex` that the blocked thread holds for the whole
syscall. So the parked thread cannot be interrupted, and no other thread can
reach the socket through the stream — which is what "the wakeup has to be
expressed against the underlying socket" means.

**It is already expressed there.** Every registry entry carries a second,
independent handle beside the assembler:

| table | entry field |
|---|---|
| `servlet::s2_registry().tls_streams` | `TlsEntry.raw: Option<TcpStream>` — a `try_clone`d duplicate |
| `t27_tls::sreg().client_streams` | `TlsClientStreamEntry.raw: Option<TcpStream>` |
| `t27_tls::sreg().server_streams` | `TlsServerStreamEntry.raw: Option<TcpStream>` |

`raw` is reachable under the **registry** lock alone — not the stream mutex —
and `s2_tls_close` / `rustls_stream_close` already use it for their
`shutdown(Both)`. That is exactly the handle a readiness probe needs, and it
already exists. The design is therefore not "find a way to reach the socket";
it is "probe it *before* taking the stream mutex".

### The shape

Per site, and it is the same shape all four times:

1. Under the registry lock, clone the `Arc` (already done) **and** `try_clone`
   the `raw` handle, or snapshot its raw fd. Release the lock (already done).
2. Loop: re-ask the registry whether the id is still present; then poll the raw
   handle for the direction this site wants, with a bounded slice; then, only
   once ready, take the stream mutex and issue the one TLS op.
3. `ErrorKind::Interrupted` once the id is gone, which each site's existing
   `*_classify_after_block` already knows how to turn into the right Java type.

`servlet.rs` needs no new primitive: `s2_wait_ready_close_aware(fd, want_write,
&still_registered)` is already `pub(crate)` in that file, already used by
`s2_blocking_accept` and the plain stream read/write, and already carries
`S2_CLOSE_POLL_MS = 25`. `t27_tls.rs` has **no** poll binding at all and must
borrow one rather than add an eighth — either `servlet::s2_poll_ready`
(`pub(crate)`, same crate) or, now that it is `pub`,
`cratonvm_native_io::net::poll_stream_readable`.

### The four traps, in the order they will be hit

1. **`read_eof_tolerant` swallows the carrier.** `t27_tls::rustls_stream_read`'s
   *client* arm goes through
   `http_url_connection::read_eof_tolerant`, which is a retry loop over
   `ErrorKind::Interrupted`. `Interrupted` is this family's close carrier
   everywhere else. Raise the close **outside** that call — i.e. from the
   registry re-ask in step 2, before the read is issued — or the wakeup is
   swallowed by the very function it is routed through. This one is not
   theoretical: it compiles, it looks right, and it produces the pre-fix
   behaviour.
2. **Do not abandon a read mid-record.** The assembler keeps its state behind
   its own mutex and the loop above never touches it until the socket is ready,
   so this is satisfied by construction — but only because the probe is on
   `raw` and not on the stream. A probe that took the stream mutex to reach
   `get_ref()` would serialise against the parked thread and deadlock.
3. **`raw` is an owned `TcpStream`, not an `Arc`.** `s2_tls_close` /
   `rustls_stream_close` **remove** the entry, which drops that handle. A raw fd
   integer snapshotted before the close is a use-after-close the instant the OS
   recycles the number — the same defect `pipe.rs` had and needed
   `in_flight`/`close_pending` to fix. Either `try_clone()` the handle so the
   poller owns one (cheapest, and it is what the parked thread already does for
   the assembler), or give these entries the same in-flight discipline. Do not
   snapshot the integer.
4. **A 30 s timeout is already masking the row.** Accepted server sockets and
   client dials both get `set_read_timeout(Some(30s))`/`set_write_timeout`, so a
   parked TLS read today unwedges after ~30 s with a **timeout error**, not a
   close classification. Two consequences: a probe that waits less than 30 s
   sees the defect and one that waits longer does not, and any deadline arm
   added must be derived from that existing socket timeout rather than invented
   — the `SO_RCVTIMEO` rule this record already states per regime.

**A fifth trap was found on the third pass, it is not implied by any of the four
above, and it is the one that makes the shape as written WRONG rather than
merely unverified — see "Third pass" below before implementing step 2.**

### Still NOT applied, 2026-08-12 — a second lane read the design and declined

The lane that landed W2-2's poll collapse and W7-8 §6 owns all four files and had
the budget question put to it explicitly. It declined, and the reasons are
additive to the ones below rather than a restatement:

* **Trap 1 is worse than "hit first"; it is hit *silently*.**
  `http_url_connection::read_eof_tolerant` retries on `ErrorKind::Interrupted`,
  which is this family's close carrier at every one of the other nineteen sites.
  A wakeup routed through it does not fail loudly — it produces the pre-fix
  behaviour, on a code path that now *looks* close-aware. The correct placement
  (raise from the registry re-ask, before the read is issued) is stated in the
  design and is not hard; what makes it a refusal is that **nothing in the tree
  would catch getting it wrong.** `probes/AsyncCloseProbe.java` has no TLS row,
  and it is itself unscheduled.
* **Trap 3 is a use-after-close, and the cheap fix has a cost this lane could not
  price.** `try_clone()`ing `raw` under the registry lock is right, and it is
  what the parked thread already does for the assembler — but it duplicates a
  descriptor per blocking operation on a path that Tomcat/WildFly drive at
  request rate. Whether that is free or a descriptor leak under load is a
  measurement, and the alternative (in-flight/close-pending discipline, as
  `pipe.rs` needed) is a larger change to three registries.
* **Trap 4 means the row is currently *masked*, not *hanging*.** The existing
  30 s `set_read_timeout` unwedges a parked TLS read with a timeout error. That
  is the wrong classification, but it is not a hang, so the cost of leaving this
  open is bounded in a way the pre-fix `pipe.rs` and `SocketInputStream` rows
  were not. Ordering the work by that difference is deliberate.

One thing that did change in this lane's favour and is worth recording, because
it removes an obstacle the design named: `t27_tls.rs` no longer has to choose
between `servlet::s2_poll_ready` and adding a binding —
`cratonvm_native_io::net::poll_stream_readable` and `poll_stream_writable` are
`pub`, and `net_phase_e.rs` has just demonstrated the call across the crate
boundary. The remaining work is the `raw` lifetime and the placement of the
raise, not the primitive.

### Why it was still not applied here

Three of the four traps are invisible to a compiler, the fourth (`raw` lifetime)
is a use-after-close, and the row's own instrument — `probes/AsyncCloseProbe.java`
— has **no TLS row**. Landing a wakeup whose only evidence is that it compiles is
the thing this record refused to do for the Windows pipe write, and the same
refusal applies here. What is now different from "not one to solve without a
build" is that the build has something specific to run: the design above, plus
two new `AsyncCloseProbe` rows (`tlsRead`, `tlsWrite`) built the way the
existing twelve are — prove the park first, bound every wait, `INCONCLUSIVE`
rather than `PASS` for a row that returned early.

### Third pass, 2026-08-12 — the shape above is UNSOUND AS WRITTEN

A third lane owning all four files re-derived the design against the two TLS
crates' own sources (`native-tls 0.2.18` and `rustls 0.23.42`, both present in
this host's cargo registry and quoted below) and found a **fifth trap that none
of the four implies**. It is worse than all of them, because the four are reasons
the fix is hard to *verify* and this one is a reason the fix as written is
*wrong*:

> **Socket readiness is not stream readiness.** Step 2 — *"poll the raw handle
> for the direction this site wants … then, only once ready, take the stream
> mutex and issue the one TLS op"* — **parks a read that would have returned
> immediately.**

TLS decrypts a whole record at a time: one fragment carries up to 16 KiB of
plaintext, and several fragments can arrive in a single segment. A caller that
asks for fewer bytes than the assembler has already decrypted leaves the
remainder buffered **inside the assembler**, and the next read must hand those
bytes back with **no socket I/O at all**. rustls says so in its own source —
`Stream::prepare_read` (`rustls-0.23.42/src/stream.rs`) touches the transport
only `while self.conn.wants_read()`, and `wants_read`
(`rustls-0.23.42/src/common_state.rs`) is

```rust
self.received_plaintext.is_empty()
    && !self.has_received_close_notify
    && (self.may_send_application_data || self.sendable_tls.is_empty())
```

so buffered plaintext means no `read_tls`, no `recv`, and an immediate return.
Gate that on socket readability and the reader waits for an edge that will never
arrive: the peer has already sent everything it intends to send until it gets a
reply this reader is now never going to produce. That is a **deadlock introduced
on the most ordinary HTTP-over-TLS shape**, not on an edge case, and
`servlet.rs`'s 32 KiB readahead does not screen it — four maximal fragments in
one segment leave more than 32 KiB behind.

This is the same species as the refusal this record already made for the Windows
pipe sink write, one step further along: there, answering `Some(Ok(false))` would
have spun a loop that could never report readiness. Here, polling a socket for a
byte the record layer does not need would park a reader that had its answer in
hand. **The standing rule gains a second clause: do not fabricate a readiness
question the layer above does not answer.**

#### The screen exists on both stacks, and it is behind the wrong lock

The gate is only sound if it is preceded by a question to the **assembler**, not
to the socket. Both stacks can answer it:

| site | screen | meaning |
|---|---|---|
| `t27_tls`'s client arm, `StreamOwned<ClientConnection, TcpStream>` | `conn.wants_read()` | `false` ⇒ this read will do no socket I/O |
| `t27_tls`'s server arm / `servlet.rs`, `native_tls::TlsStream<TcpStream>` | `buffered_read_size()` (`native-tls-0.2.18/src/lib.rs`) | `> 0` ⇒ readable without touching the network |
| `t27_tls`'s `LegacyDsa` arm, `openssl::ssl::SslStream` | openssl's own pending-bytes query, `#[cfg(unix)]` | **not writable from this host** — see below |

The `wants_read()` direction is exact rather than approximate, which is why it is
usable: when it is `false`, `prepare_read` does not loop at all and
`reader().read(buf)` returns from the buffer; when it is `true`, `complete_io`
runs and the call can park. So the correct order is **screen under the stream
mutex → release it → poll `raw` on a bounded slice with the registry re-ask →
retake the mutex → read**. Taking the mutex first is not a new hazard: every one
of these four sites already takes it for the whole call, so a second reader
already queues behind a parked one today.

#### Trap 3 is retired, and the fix is cheaper than the design assumed

`try_clone()`ing `raw` per blocking operation was the cost the second lane could
not price. **It is not needed.** The nineteen fixed sites do not `try_clone`
anything: they park holding an `Arc<TcpStream>` cloned out of a registry, and it
is the `Arc` — not a duplicated descriptor — that stops the handle being freed
under a parked thread. Give these three registries the same discipline:

| table | today | should be |
|---|---|---|
| `servlet::TlsEntry.raw` | `Option<TcpStream>` | `Option<Arc<TcpStream>>` |
| `t27_tls::TlsClientStreamEntry.raw` | `Option<TcpStream>` | `Option<Arc<TcpStream>>` |
| `t27_tls::TlsServerStreamEntry.raw` | `Option<TcpStream>` | `Option<Arc<TcpStream>>` |

`shutdown` takes `&self`, so both closes keep working through the `Arc`
unchanged; `cratonvm_native_io::net::poll_stream_readable(&TcpStream, i32)` takes
`&TcpStream`, which an `Arc` derefs to. A reader then clones an `Arc` (no
syscall) instead of duplicating a descriptor (two syscalls and a handle) per
read, and the use-after-close trap 3 names cannot occur because the handle
outlives every holder. The remaining cost is **one `poll` on a read that was
going to go to the socket anyway** — the same price
`net::net_read_close_aware` already accepted in this family ("it costs one extra
syscall on a read that would have blocked anyway").

#### The residual that survives even the correct fix

A maximal TLS record is ~16 KiB and a TCP segment is ~1.5 KiB, so
`prepare_read`'s `while wants_read()` loop calls `complete_io` repeatedly and
**parks inside it** between the segments of one record. The screened design makes
a reader close-aware while the record layer is at rest — which is where the hang
matters, an idle keep-alive connection waiting for the next request — and leaves
it unwakeable mid-record. That is not a hole in the design; it is the same
statement the design already makes about not abandoning a read mid-record, priced
honestly. Anyone counting this row closed must count that residual with it.

#### Why this lane did not land it either

1. **The whole benefit is on Windows, and every arm of the evidence is
   unavailable here.** On Unix the `raw` shutdown W7-61 landed already delivers
   the wakeup. This adds the Windows arm — on a host with no build, against an
   instrument (`probes/AsyncCloseProbe.java`) that is still not scheduled.
2. **The blast radius is every TLS read in the VM.** These two functions carry
   Tomcat, WildFly, the Spring TLS slices and H2-over-TLS. A screen that is
   wrong in the `false` direction is a hang on every read, and the failure mode
   is indistinguishable from the defect being fixed.
3. **The `LegacyDsa` arm cannot be written from this host at all.** It is
   `#[cfg(unix)]`, nothing here type-checks it, and its screen would be an API
   guess — exactly the shape this record refused for the pipe write.
4. **Trap 4 still bounds the cost of waiting.** The row is masked by a 30 s
   socket timeout, not hanging, so the asymmetry is between a bounded wrong
   classification and an unbounded hang.

**The pilot a build lane should take first, because it needs none of the above.**
`t27_tls::rustls_stream_read`'s **client arm only**: one struct (`raw` to
`Option<Arc<TcpStream>>`, two construction sites), one screen (`conn.wants_read()`,
proved exact above), no enum variants, and **no platform-specific code of any
kind** — `StreamOwned<ClientConnection, TcpStream>` has no `cfg` arms. It is the
one site where the entire mechanism is expressible in safe, portable, compilable
Rust, and running `AsyncCloseProbe`'s `tlsRead` + `tlsReadIntegrity` rows against
it decides the design for the other three. Do the pilot; do not do all four at
once.

### Fourth pass, 2026-08-12 — the design is now WRITTEN DOWN, and one of its
### citations was against the wrong crate version

**`docs/feature-designs/jdk-only-tls-async-close.md`** is the P3-D design
document the roadmap asked for. It supersedes this section as the place to read
before implementing; this section stays because it is the history of how the
shape was arrived at. Still **nothing applied** — no file named in any of the
four passes has been edited.

Four corrections and additions this record should carry:

1. **The third pass quoted `rustls-0.23.42`. This tree builds `rustls-0.23.38`**
   (`Cargo.lock`). The evidence was read off a version that is not shipped. It
   was re-derived against 0.23.38 and `prepare_read` / `wants_read` are
   **byte-identical**, so the conclusion stands — but a reader checking the quote
   against the tree would have found nothing at the cited path.
2. **`wants_read()` is exact in BOTH directions, and the third pass only argued
   one.** The unaddressed worry is a false `true`: rustls holding a fully
   received but undeframed record while `received_plaintext` is empty. It cannot
   happen — `ConnectionCommon::complete_io` (`rustls-0.23.38/src/conn.rs:602`)
   runs `process_new_packets()` after every pass of its read loop, so at the
   instant any `complete_io` returns everything deframable is already deframed.
   That is what makes the pilot expressible at all.
3. **The native-tls screen does NOT have the same standing, and the third pass's
   table implies it does.** `buffered_read_size()` is documented as "bytes that
   can be read without resulting in any network calls", but the Windows backend
   is `Ok(self.0.get_buf().len())`
   (`native-tls-0.2.18/src/imp/schannel.rs:394`) and schannel's `get_buf` returns
   the **decrypted** buffer only — bytes in the *encrypted* input buffer are not
   counted. A false zero there is the deadlock direction. So the two screens are
   not interchangeable and the other three sites must not be done "the same way".
4. **The workspace is edition 2021**, so the `if let` scrutinee-temporary hazard
   is live in these crates today: a guard bound in an `if let` scrutinee is alive
   inside the `else`, which is where the poll would go. Do not plan around an
   edition bump — `match` scrutinee temporaries live for the whole `match` in
   *every* edition. The rule is structural: bind the guard with an explicit
   `let`, `drop(guard)` explicitly, then branch.

The design also names a **row that does not exist**: nothing in
`AsyncCloseProbe` currently exercises "peer writes more than one `read` request
and then goes silent", which is the exact shape the unsound gate deadlocks on.
`tlsReadBufferedRemainder` has to be written **before** the pilot lands, not
after.

## The Windows pipe sink write, narrowed — the row stays OPEN

`native-io/src/pipe.rs` is in the lane that wrote this update, so this one was
edited. **It is not fixed and the row above still says open.** What changed:

`poll_pipe(raw, /* want_write */ true, …)` still answers `None` on Windows, for
all the reasons already given, and `pipe_write_close_aware`'s `None` arm still
routes to a plain blocking `WriteFile`. It used to hand that write **the entire
remainder** in one call, so a close arriving during a large payload was
unobservable for the whole payload: the generation check ran once and the thread
was then gone for the duration. The `None` arm now slices at
`PIPE_WRITE_SLICE_MAX` (4 KiB, the `CreatePipe` default buffer size, and the same
constant the `Some(Ok(true))` arm already used), so `pipe_still_open` is re-asked
between slices.

Stated exactly, because this is the kind of change that gets miscounted as a
close: the blind window is now **one slice** — as long as the reader takes to
drain 4 KiB — instead of as long as it takes to drain everything. Against a
reader that has stopped entirely, the first slice still parks forever. That is
the residual, it is the same residual, and the named follow-up
(`CreateNamedPipe(FILE_FLAG_OVERLAPPED)` + a bounded `GetOverlappedResultEx`) is
unchanged. The argument for slicing is not new either: it is `net.rs`'s
`NET_WRITE_SLICE_MAX`, *"a single unsliced `send` parks for an unbounded time and
observes no close"*, applied to the one site in this family that had not taken
it.

One behavioural detail worth knowing before reading the diff: the `None` arm used
to `return` after one write, so it could report a short count; it now loops to
completion like the polled arm, and answers a partial count only when a write
accepts nothing without reporting an error. Looping there rather than spinning
matters because that arm has no poll to bound a retry.

---

## Platform

**No row here is described as Windows-only or Unix-only.** The host is Windows
and no Linux arm was run, so a platform claim would be inference rather than
evidence — the same refusal W7-47 made and for the same reason.

What *is* platform-specific is stated structurally rather than empirically, and
only where the code forces it:

* `shutdown` semantics differ, and this is a documented OS contract rather than
  an observation: on Linux `shutdown(SHUT_RD)` wakes a parked `recv` with EOF
  (still the wrong answer where the spec mandates an exception, but not a hang);
  Winsock has no `shutdown` that aborts a pending blocking call. That difference
  is why several of these rows would read as "returns -1" on one platform and
  "never returns" on the other, and why the `async_socket.rs` comment quoted
  above is true where it was written and false where it now runs.
* `poll(2)` is never restarted by `SA_RESTART`; Winsock has no EINTR. Both are
  contracts, not measurements, and each Unix arm added here carries the EINTR
  branch that follows from the first.
* The `#[cfg(unix)]` arms added in `fd_table.rs`, `net_phase_e.rs` and `pipe.rs`
  are **not compilable on this host**, in principle as well as in practice. The
  `#[cfg(not(any(unix, windows)))]` arms are not compilable on any host in this
  campaign.

---

## Flags

None added. `probe.skipClose`, `probe.settleMs`, `probe.wakeMs` and
`probe.enterMs` are Java system properties read by the probe itself, not
`CRATONVM_*` VM flags, so no `flag_groups.rs` / `flag-surface.txt` /
`flag-tokens.md` / `flag-inventory.md` work is implied.

## Tests

No existing test was weakened. Three were added, in `native-io/src/pipe.rs`,
covering the mechanism this lane invented rather than reused.

Added 2026-08-12, and it is the first **scheduled** cover this family has ever
had beyond the single read row: `regression-suite/src/RJdkNet.java`'s
`asyncCloseWriteAndAccept()`, 8 checks, 72 → **80**. See "Two of the thirteen
shapes are now SCHEDULED" above for what each row proves, why neither can hang,
and why there is no TLS row.
